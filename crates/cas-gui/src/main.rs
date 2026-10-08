use cas_core::{AccountRecord, Cas};
use gpui::{
    App, Application, Bounds, Context, PathPromptOptions, Render, SharedString, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};

struct AccountWindow {
    cas: Cas,
    accounts: Vec<AccountRecord>,
    active_id: Option<String>,
    current_label: String,
    busy: Option<String>,
    message: Option<String>,
}

impl AccountWindow {
    fn new(cas: Cas) -> Self {
        let mut this = Self {
            cas,
            accounts: Vec::new(),
            active_id: None,
            current_label: "No active Codex account".into(),
            busy: None,
            message: None,
        };
        this.reload();
        this
    }

    fn reload(&mut self) {
        match self.cas.list_accounts() {
            Ok(accounts) => self.accounts = accounts,
            Err(error) => self.message = Some(error.to_string()),
        }
        match self.cas.current_account() {
            Ok(current) => {
                self.active_id = current.account.as_ref().map(|a| a.id.clone());
                self.current_label = match (current.account, current.identity) {
                    (Some(account), _) => format!("Current: {}", account.display_name()),
                    (None, Some(identity)) => {
                        format!("Current: {} (not saved)", identity.display_name())
                    }
                    (None, None) => "No active Codex account".into(),
                };
            }
            Err(error) => {
                self.active_id = None;
                self.current_label = "Unable to read current Codex account".into();
                self.message = Some(error.to_string());
            }
        }
    }

    fn start_login(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Starting device login...".into());
        self.message = None;
        cx.notify();

        let cas = self.cas.clone();
        let start_cas = cas.clone();
        let task = cx
            .background_executor()
            .spawn(async move { start_cas.begin_device_login() });
        cx.spawn(async move |this, cx| {
            let login = match task.await {
                Ok(login) => login,
                Err(error) => {
                    let _ = this.update(cx, |view, cx| {
                        view.busy = None;
                        view.message = Some(error.to_string());
                        cx.notify();
                    });
                    return;
                }
            };

            let verification_url = login.verification_url.clone();
            let user_code = login.user_code.clone();
            let _ = this.update(cx, |view, cx| {
                view.busy = Some("Waiting for device login...".into());
                view.message = Some(format!(
                    "Open {verification_url} and enter code {user_code}. CAS will not open a browser or start Codex."
                ));
                cx.notify();
            });

            let finish = cx
                .background_executor()
                .spawn(async move { cas.complete_device_login(login, None) });
            let result = finish.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = None;
                match result {
                    Ok(account) => {
                        view.message = Some(format!(
                            "Saved {}. The active Codex account was not changed.",
                            account.display_name()
                        ));
                        view.reload();
                    }
                    Err(error) => view.message = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_import_current(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Importing current account...".into());
        self.message = None;
        cx.notify();

        let cas = self.cas.clone();
        let task = cx
            .background_executor()
            .spawn(async move { cas.import_current(None) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = None;
                match result {
                    Ok(account) => {
                        view.message = Some(format!("Saved {}.", account.display_name()));
                        view.reload();
                    }
                    Err(error) => view.message = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_import_file(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Choosing auth.json...".into());
        self.message = None;
        cx.notify();

        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select auth.json".into()),
        });
        let cas = self.cas.clone();
        cx.spawn(async move |this, cx| {
            let path = match picker.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => Some(paths.remove(0)),
                Ok(Ok(_)) => None,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.busy = None;
                        view.message = Some(error.to_string());
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let _ = this.update(cx, |view, cx| {
                        view.busy = None;
                        view.message = Some(format!("File picker failed: {error}"));
                        cx.notify();
                    });
                    return;
                }
            };

            let Some(path) = path else {
                let _ = this.update(cx, |view, cx| {
                    view.busy = None;
                    view.message = Some("Import cancelled.".into());
                    cx.notify();
                });
                return;
            };

            let task = cx
                .background_executor()
                .spawn(async move { cas.input(Some(&path)) });
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = None;
                match result {
                    Ok(account) => {
                        view.message = Some(format!("Saved {}.", account.display_name()));
                        view.reload();
                    }
                    Err(error) => view.message = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_switch(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let label = self
            .accounts
            .iter()
            .find(|account| account.id == id)
            .map(AccountRecord::display_name)
            .unwrap_or_else(|| id.clone());
        self.busy = Some(format!("Switching to {label}..."));
        self.message = None;
        cx.notify();

        let cas = self.cas.clone();
        let task = cx
            .background_executor()
            .spawn(async move { cas.switch_id(&id) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = None;
                match result {
                    Ok(result) => {
                        view.message = Some(format!(
                            "Switched to {}. Codex is stopped and was not relaunched.",
                            result.to.display_name()
                        ));
                        view.reload();
                    }
                    Err(error) => view.message = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_status(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Refreshing account status...".into());
        self.message = None;
        cx.notify();

        let cas = self.cas.clone();
        let task = cx
            .background_executor()
            .spawn(async move { cas.status(None) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = None;
                match result {
                    Ok(statuses) => {
                        view.message = Some(format!(
                            "Refreshed status for {} account(s).",
                            statuses.len()
                        ));
                        view.reload();
                    }
                    Err(error) => view.message = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for AccountWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let disabled = self.busy.is_some();

        let login = div()
            .id("login")
            .px_3()
            .py_2()
            .rounded_md()
            .bg(if disabled {
                rgb(0x30343b)
            } else {
                rgb(0x2563eb)
            })
            .text_color(rgb(0xffffff))
            .cursor_pointer()
            .child("Login")
            .on_click(cx.listener(|this, _, _, cx| {
                if this.busy.is_none() {
                    this.start_login(cx);
                }
            }));

        let import = div()
            .id("import-current")
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x3f4652))
            .text_color(rgb(0xd9dde5))
            .cursor_pointer()
            .child("Import current")
            .on_click(cx.listener(|this, _, _, cx| {
                if this.busy.is_none() {
                    this.start_import_current(cx);
                }
            }));

        let refresh = div()
            .id("status")
            .px_3()
            .py_2()
            .rounded_md()
            .text_color(rgb(0xaab1bd))
            .cursor_pointer()
            .child("Status")
            .on_click(cx.listener(|this, _, _, cx| {
                if this.busy.is_none() {
                    this.start_status(cx);
                }
            }));

        let import_file = div()
            .id("import-file")
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x3f4652))
            .text_color(rgb(0xd9dde5))
            .cursor_pointer()
            .child("Import auth.json")
            .on_click(cx.listener(|this, _, _, cx| {
                if this.busy.is_none() {
                    this.start_import_file(cx);
                }
            }));

        let mut list = div().flex().flex_col().gap_2();
        if self.accounts.is_empty() {
            list = list.child(
                div()
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x303640))
                    .text_color(rgb(0x9da5b2))
                    .child("No saved accounts."),
            );
        } else {
            for account in self.accounts.clone() {
                let id = account.id.clone();
                let is_active = self.active_id.as_deref() == Some(account.id.as_str());
                let subtitle = account
                    .email
                    .clone()
                    .filter(|email| *email != account.display_name())
                    .unwrap_or_else(|| format!("id {}", &account.id[..8.min(account.id.len())]));
                let status_line = account.last_status.as_ref().map(|status| {
                    let mut parts = vec![match status.valid {
                        Some(true) => "valid".to_string(),
                        Some(false) => "invalid".to_string(),
                        None => "unknown".to_string(),
                    }];
                    if let Some(window) = status.five_hour.as_ref() {
                        parts.push(format!("5h {}%", window.remaining_percent));
                    }
                    if let Some(window) = status.long_window.as_ref() {
                        parts.push(format!("{} {}%", window.label(), window.remaining_percent));
                    }
                    parts.join(" • ")
                });

                let action = if is_active {
                    div()
                        .id(SharedString::from(format!("active-{id}")))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x183a2b))
                        .text_color(rgb(0x8ce0ad))
                        .child("Active")
                } else {
                    div()
                        .id(SharedString::from(format!("switch-{id}")))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(0x46505f))
                        .text_color(rgb(0xe5e7eb))
                        .cursor_pointer()
                        .child(if disabled { "Busy" } else { "Switch" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.busy.is_none() {
                                this.start_switch(id.clone(), cx);
                            }
                        }))
                };

                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .p_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(if is_active {
                            rgb(0x2f6f50)
                        } else {
                            rgb(0x303640)
                        })
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_color(rgb(0xf3f4f6))
                                        .child(account.display_name()),
                                )
                                .child(div().text_sm().text_color(rgb(0x8f98a6)).child(subtitle))
                                .when_some(status_line, |this, status| {
                                    this.child(
                                        div().text_sm().text_color(rgb(0x8f98a6)).child(status),
                                    )
                                }),
                        )
                        .child(action),
                );
            }
        }

        let mut root = div()
            .size_full()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe8ebf0))
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xl().child("CAS"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x929aa7))
                                    .child(self.current_label.clone()),
                            ),
                    )
                    .child(login),
            )
            .child(list)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(import)
                    .child(import_file)
                    .child(refresh),
            );

        if let Some(busy) = &self.busy {
            root = root.child(
                div()
                    .text_sm()
                    .text_color(rgb(0xa8c5ff))
                    .child(busy.clone()),
            );
        }
        if let Some(message) = &self.message {
            root = root.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(0x1a1e25))
                    .text_sm()
                    .text_color(rgb(0xcbd1dc))
                    .child(message.clone()),
            );
        }
        root
    }
}

fn main() {
    let cas = match Cas::discover() {
        Ok(cas) => cas,
        Err(error) => {
            eprintln!("cas-gui: {error}");
            std::process::exit(1);
        }
    };

    Application::new().run(move |cx: &mut App| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(640.0), px(560.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("CAS — ChatGPT Account Switcher".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| AccountWindow::new(cas.clone())),
        )
        .expect("failed to open CAS window");
        cx.activate(true);
    });
}
