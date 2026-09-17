use std::any::TypeId;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use command_palette_hooks::CommandPaletteFilter;
use db::kvp::KeyValueStore;
use gpui::{DismissEvent, actions};
use system_specs::{FlatpakState, flatpak_state};
use ui::prelude::*;
use util::ResultExt as _;
use util::command::new_command;
use workspace::MultiWorkspace;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::notifications::{NotificationId, NotifyResultExt as _, show_app_notification};

/// The launcher shipped inside the sandbox, which restarts Zed on the host when
/// `ZED_FLATPAK_ESCAPE` is set. See the `flatpak` module in `crates/cli/src/main.rs`.
const SANDBOX_CLI_PATH: &str = "/app/bin/zed";
const ESCAPE_ENV_NAME: &str = "ZED_FLATPAK_ESCAPE";
const DISMISSED_KEY: &str = "flatpak_sandbox_notice_dismissed";
const DOCS_URL: &str = "https://zed.dev/docs/linux#flatpak";

actions!(
    flatpak,
    [
        /// Restarts Zed on the host system, outside the Flatpak sandbox, for this launch only.
        RestartOnHost,
        /// Always starts Zed on the host system, outside the Flatpak sandbox.
        AlwaysRunOnHost,
        /// Always starts Zed inside the Flatpak sandbox.
        AlwaysRunInSandbox,
    ]
);

/// Must run after `command_palette::init`, which installs the filter these actions are hidden with.
pub fn init(cx: &mut App) {
    let state = flatpak_state();

    CommandPaletteFilter::update_global(cx, |filter, _| match state {
        FlatpakState::NotFlatpak => filter.hide_action_types(&[
            TypeId::of::<RestartOnHost>(),
            TypeId::of::<AlwaysRunOnHost>(),
            TypeId::of::<AlwaysRunInSandbox>(),
        ]),
        // Restarting onto the host is meaningless when already there.
        FlatpakState::EscapedToHost { .. } => {
            filter.hide_action_types(&[TypeId::of::<RestartOnHost>()])
        }
        FlatpakState::Sandboxed { .. } => {}
    });

    if state == &FlatpakState::NotFlatpak {
        return;
    }

    cx.on_action(|_: &RestartOnHost, cx| restart_on_host(cx));
    cx.on_action(|_: &AlwaysRunOnHost, cx| set_always_run_on_host(true, cx));
    cx.on_action(|_: &AlwaysRunInSandbox, cx| set_always_run_on_host(false, cx));

    if state.is_sandboxed() {
        show_sandbox_notice(cx);
    }
}

fn show_sandbox_notice(cx: &mut App) {
    let kvp = KeyValueStore::global(cx);
    if matches!(kvp.read_kvp(DISMISSED_KEY), Ok(Some(value)) if value == "true") {
        return;
    }

    struct FlatpakSandboxNotice;

    show_app_notification(NotificationId::unique::<FlatpakSandboxNotice>(), cx, |cx| {
        let notification = cx.new(|cx| {
            MessageNotification::new(
                "Zed only has access to the tools and files that the sandbox exposes. \
                 Restart on the host to use your full development environment.",
                cx,
            )
            .with_title("Running in the Flatpak Sandbox")
            .primary_message("Restart on Host")
            .primary_on_click(|_window, cx| restart_on_host(cx))
            .secondary_message("Always Run on Host")
            .secondary_on_click(|_window, cx| set_always_run_on_host(true, cx))
            .more_info_message("Learn More")
            .more_info_url(DOCS_URL)
        });

        // This is a first-run notice, so dismissing it counts as having read it.
        cx.subscribe(&notification, |_, _, _: &DismissEvent, cx| {
            let kvp = KeyValueStore::global(cx);
            cx.background_spawn(async move {
                kvp.write_kvp(DISMISSED_KEY.to_string(), "true".to_string())
                    .await
                    .log_err();
            })
            .detach();
        })
        .detach();

        notification
    });
}

/// Restarting waits for this process to exit before running `program`, which matters because
/// both ways back in hand off to any Zed still listening on the CLI socket.
fn restart(program: &str, arguments: Vec<OsString>, cx: &mut App) {
    let program = PathBuf::from(program);
    cx.spawn(async move |cx| {
        let windows = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<MultiWorkspace>())
                .collect::<Vec<_>>()
        });
        if !workspace::prepare_windows_to_quit(&windows, cx).await {
            return;
        }

        cx.update(|cx| {
            cx.set_restart_path(program);
            cx.set_restart_arguments(arguments);
            cx.restart();
        });
    })
    .detach();
}

fn restart_on_host(cx: &mut App) {
    // `env` carries the opt-out to the launcher without mutating this process's environment.
    restart(
        "/usr/bin/env",
        vec![
            format!("{ESCAPE_ENV_NAME}=1").into(),
            SANDBOX_CLI_PATH.into(),
        ],
        cx,
    );
}

fn set_always_run_on_host(enabled: bool, cx: &mut App) {
    cx.spawn(async move |cx| {
        let written = write_always_run_on_host(enabled).await;
        cx.update(|cx| {
            if written.notify_app_err(cx).is_none() {
                return;
            }

            // An override only takes effect on the next launch, so make this that launch.
            match flatpak_state() {
                FlatpakState::Sandboxed { .. } if enabled => restart_on_host(cx),
                FlatpakState::EscapedToHost { app_id } if !enabled => {
                    restart("/usr/bin/flatpak", vec!["run".into(), app_id.into()], cx)
                }
                _ => {}
            }
        });
    })
    .detach();
}

/// Zed's launcher reads `ZED_FLATPAK_ESCAPE` before Zed starts, so the choice has to live
/// somewhere the launcher can see. A Flatpak override is that place, and it keeps the choice
/// visible to `flatpak override --show` and revertible without Zed.
async fn write_always_run_on_host(enabled: bool) -> Result<()> {
    let state = flatpak_state();
    let app_id = state.app_id().context("not running as a Flatpak")?;
    let argument = if enabled {
        format!("--env={ESCAPE_ENV_NAME}=1")
    } else {
        format!("--unset-env={ESCAPE_ENV_NAME}")
    };

    // `flatpak` only exists on the host, so reach it back out through the sandbox when needed.
    let mut command = if state.is_sandboxed() {
        let mut command = new_command("/usr/bin/flatpak-spawn");
        command.arg("--host").arg("flatpak");
        command
    } else {
        new_command("/usr/bin/flatpak")
    };

    let output = command
        .args(["override", "--user", argument.as_str(), app_id])
        .output()
        .await
        .context("failed to run flatpak override")?;

    anyhow::ensure!(
        output.status.success(),
        "flatpak override failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    Ok(())
}

/// What the About window shows for this install, or `None` when Zed is not a Flatpak.
pub fn about_window_status() -> Option<SharedString> {
    match flatpak_state() {
        FlatpakState::Sandboxed { .. } => Some("Flatpak sandbox".into()),
        FlatpakState::EscapedToHost { .. } => Some("Flatpak, running on host".into()),
        FlatpakState::NotFlatpak => None,
    }
}
