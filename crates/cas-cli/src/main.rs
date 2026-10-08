use cas_core::{AccountChoice, AccountStatus, Cas, CasError, UsageWindow};
use chrono::{DateTime, Local, Utc};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    style::{Attribute, Color, Print, SetAttribute, Stylize, style},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

fn zh() -> bool {
    cas_core::ui_is_chinese()
}

fn color_enabled() -> bool {
    io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").ok().as_deref() != Some("dumb")
}

fn paint(text: impl AsRef<str>, color: Color) -> String {
    let text = text.as_ref();
    if color_enabled() {
        format!("{}", style(text).with(color))
    } else {
        text.to_owned()
    }
}

fn label(text: impl AsRef<str>) -> String {
    paint(text, Color::Cyan)
}

fn dim(text: impl AsRef<str>) -> String {
    paint(text, Color::DarkGrey)
}

fn separator() -> String {
    dim(" | ")
}

fn usage_color(percent: i32) -> Color {
    let percent = percent.clamp(0, 100);
    let (r, g) = if percent <= 50 {
        (255, ((percent * 255 + 25) / 50) as u8)
    } else {
        ((((100 - percent) * 255 + 25) / 50) as u8, 255)
    };
    Color::Rgb { r, g, b: 0 }
}

fn usage_value(percent: i32) -> String {
    paint(format!("{percent}%"), usage_color(percent))
}

fn unavailable_usage(label_text: &str) -> String {
    let value = if zh() { "不可用" } else { "unavailable" };
    format!("{}{}{}", label(label_text), dim("="), dim(value))
}

fn field(label_text: &str, value: &str, color: Color) -> String {
    format!("{}{}{}", label(label_text), dim("="), paint(value, color))
}

#[derive(Debug, Parser)]
#[command(name = "cas", version, about = "ChatGPT Account Switcher for Codex")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Log in to a ChatGPT account using browser OAuth or device-code authentication.
    Login {
        /// Login method. With no method, open an interactive selector.
        method: Option<LoginMethod>,
    },
    /// Refresh credential validity and Codex usage. With no target, refresh every saved account.
    Status {
        /// Case-insensitive email prefix. Multiple matches open the account selector.
        account: Option<String>,
    },
    /// Concurrently send a clean streaming hello with every saved account.
    #[command(name = "test/refresh", aliases = ["test", "refresh"])]
    TestRefresh,
    /// Stop Codex and switch accounts. With no target, open an interactive selector.
    #[command(name = "switch/enable", aliases = ["switch", "enable"])]
    Switch {
        /// Case-insensitive email prefix. Multiple matches open the account selector.
        account: Option<String>,
    },
    /// Import one auth.json. With no path, import the current user's .codex/auth.json.
    #[command(name = "import/input", aliases = ["import", "input"])]
    Input {
        /// Path to one auth.json file.
        path: Option<PathBuf>,
    },
    /// Remove one saved account. With no target, open an interactive selector.
    #[command(name = "remove/delete", aliases = ["remove", "delete"])]
    Remove {
        /// Complete email address. Multiple workspace matches open the account selector.
        email: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LoginMethod {
    Browser,
    Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainAction {
    Status,
    TestRefresh,
    Switch,
    Login,
    Input,
    Remove,
    Help,
}

fn main() {
    if let Err(error) = cas_core::initialize_http_client() {
        if zh() {
            eprintln!("错误：HTTP 客户端初始化失败：{error}");
        } else {
            eprintln!("ERROR: HTTP client initialization failed: {error}");
        }
        std::process::exit(1);
    }
    if maybe_print_localized_help() {
        return;
    }
    let cli = Cli::parse();
    let command_name = cli.command.as_ref().map(Command::name).unwrap_or("cas");
    match run(cli) {
        Ok(()) => {}
        Err(error) => {
            if zh() {
                eprintln!("错误：{command_name}：{error}");
            } else {
                eprintln!("ERROR: {command_name} failed: {error}");
            }
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> cas_core::Result<()> {
    let cas = Cas::discover()?;
    let command = match cli.command {
        Some(command) => command,
        None => {
            let Some(action) = select_main_action()? else {
                return Ok(());
            };
            match action {
                MainAction::Status => Command::Status { account: None },
                MainAction::TestRefresh => Command::TestRefresh,
                MainAction::Switch => Command::Switch { account: None },
                MainAction::Login => Command::Login { method: None },
                MainAction::Input => Command::Input { path: None },
                MainAction::Remove => Command::Remove { email: None },
                MainAction::Help => {
                    print_ui_help(None);
                    return Ok(());
                }
            }
        }
    };

    match command {
        Command::Login { method } => {
            let method = match method {
                Some(method) => method,
                None => {
                    let Some(method) = select_login_method()? else {
                        return Ok(());
                    };
                    method
                }
            };
            let account = match method {
                LoginMethod::Browser => {
                    let login = cas.begin_browser_login()?;
                    if zh() {
                        println!("打开：{}", login.auth_url);
                    } else {
                        println!("Open: {}", login.auth_url);
                    }
                    match webbrowser::open(&login.auth_url) {
                        Ok(()) => {
                            if zh() {
                                println!("已打开浏览器，等待登录……");
                            } else {
                                println!("browser opened; waiting for login...");
                            }
                        }
                        Err(error) => {
                            if zh() {
                                eprintln!("浏览器启动失败：{error}");
                                println!("等待浏览器回调……");
                            } else {
                                eprintln!("browser launch failed: {error}");
                                println!("waiting for browser callback...");
                            }
                        }
                    }
                    cas.complete_browser_login(login, None)?
                }
                LoginMethod::Device => {
                    let login = cas.begin_device_login()?;
                    if zh() {
                        println!("打开：{}", login.verification_url);
                        println!("代码：{}", login.user_code);
                        println!("等待登录……");
                    } else {
                        println!("Open: {}", login.verification_url);
                        println!("Code: {}", login.user_code);
                        println!("Waiting for login...");
                    }
                    cas.complete_device_login(login, None)?
                }
            };
            let workspace =
                account
                    .account_id
                    .as_deref()
                    .unwrap_or(if zh() { "未知" } else { "unknown" });
            if zh() {
                println!(
                    "已保存 {} [工作区={}]。当前 Codex 账号未更改。",
                    account.display_name(),
                    workspace
                );
            } else {
                println!(
                    "saved {} [workspace={}]. Active Codex account was not changed.",
                    account.display_name(),
                    workspace
                );
            }
        }
        Command::Status { account } => {
            let statuses = match account {
                None => cas.status(None)?,
                Some(selector) => {
                    let matches = cas.account_choices_prefix(&selector)?;
                    let prompt = if zh() {
                        "选择要查询的账号"
                    } else {
                        "Status account"
                    };
                    let Some(account) = select_matching_account(&cas, prompt, matches, &selector)?
                    else {
                        return Ok(());
                    };
                    vec![cas.status_id(&account.account.id)?]
                }
            };
            if statuses.is_empty() {
                println!(
                    "{}",
                    if zh() {
                        "没有已保存的账号。"
                    } else {
                        "no saved accounts."
                    }
                );
            } else {
                for status in &statuses {
                    println!("{}", format_status(status));
                }
                if zh() {
                    println!("已刷新 {} 个账号的状态。", statuses.len());
                } else {
                    println!("refreshed status for {} account(s).", statuses.len());
                }
            }
            println!("{}", format_current_auth(&cas.current_account()?));
        }
        Command::TestRefresh => {
            let results = cas.test_refresh_all()?;
            if results.is_empty() {
                println!(
                    "{}",
                    if zh() {
                        "没有已保存的账号。"
                    } else {
                        "no saved accounts."
                    }
                );
            } else {
                for result in &results {
                    print_test_result(result);
                }
            }
        }
        Command::Switch { account } => {
            let account = match account.as_deref() {
                Some(selector) => {
                    let matches = cas.account_choices_prefix(selector)?;
                    let prompt = if zh() {
                        "选择要切换的账号"
                    } else {
                        "Switch account"
                    };
                    let Some(account) = select_matching_account(&cas, prompt, matches, selector)?
                    else {
                        return Ok(());
                    };
                    account
                }
                None => {
                    let choices = cas.account_choices()?;
                    if choices.is_empty() {
                        println!(
                            "{}",
                            if zh() {
                                "没有已保存的账号。"
                            } else {
                                "no saved accounts."
                            }
                        );
                        return Ok(());
                    }
                    let prompt = if zh() {
                        "选择要切换的账号"
                    } else {
                        "Switch account"
                    };
                    let Some(account) = select_account(&cas, prompt, &choices)? else {
                        return Ok(());
                    };
                    account
                }
            };
            let result = cas.switch_id(&account.account.id)?;
            let target_name = result
                .to
                .email
                .clone()
                .unwrap_or_else(|| result.to.display_name());
            let workspace =
                result
                    .to
                    .account_id
                    .as_deref()
                    .unwrap_or(if zh() { "未知" } else { "unknown" });
            if zh() {
                println!(
                    "已切换到 {} [工作区={}]；退出了 {} 个进程。",
                    target_name,
                    workspace,
                    result.terminated_processes.len(),
                );
            } else {
                println!(
                    "switched to {} [workspace={}]; exited {} processes.",
                    target_name,
                    workspace,
                    result.terminated_processes.len(),
                );
            }
        }
        Command::Input { path } => {
            let source = path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| cas.paths().codex_auth_path.display().to_string());
            let account = cas.input(path.as_deref())?;
            let account_name = account
                .email
                .clone()
                .unwrap_or_else(|| account.display_name());
            if zh() {
                println!(
                    "已导入 {} ({})，来源：{}。",
                    account_name,
                    &account.id[..8.min(account.id.len())],
                    source,
                );
            } else {
                println!(
                    "imported {} ({}) from {}.",
                    account_name,
                    &account.id[..8.min(account.id.len())],
                    source,
                );
            }
        }
        Command::Remove { email } => {
            let account = match email.as_deref() {
                Some(email) => {
                    let matches = cas.account_choices_exact_email(email)?;
                    let prompt = if zh() {
                        "选择要删除的账号"
                    } else {
                        "Remove account"
                    };
                    let Some(account) = select_matching_account(&cas, prompt, matches, email)?
                    else {
                        return Ok(());
                    };
                    account
                }
                None => {
                    let choices = cas.account_choices()?;
                    if choices.is_empty() {
                        println!(
                            "{}",
                            if zh() {
                                "没有已保存的账号。"
                            } else {
                                "no saved accounts."
                            }
                        );
                        return Ok(());
                    }
                    let prompt = if zh() {
                        "选择要删除的账号"
                    } else {
                        "Remove account"
                    };
                    let Some(account) = select_account(&cas, prompt, &choices)? else {
                        return Ok(());
                    };
                    account
                }
            };
            let removed = cas.remove_id(&account.account.id)?;
            let removed_name = removed
                .email
                .clone()
                .unwrap_or_else(|| removed.display_name());
            let workspace =
                removed
                    .account_id
                    .as_deref()
                    .unwrap_or(if zh() { "未知" } else { "unknown" });
            if zh() {
                println!("已删除 {removed_name} [工作区={workspace}]。");
            } else {
                println!("removed {removed_name} [workspace={workspace}].");
            }
        }
    }

    Ok(())
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Login { .. } => "login",
            Self::Status { .. } => "status",
            Self::TestRefresh => "test/refresh",
            Self::Switch { .. } => "switch",
            Self::Input { .. } => "input",
            Self::Remove { .. } => "remove",
        }
    }
}

fn maybe_print_localized_help() -> bool {
    if !zh() {
        return false;
    }
    let args: Vec<String> = std::env::args().collect();
    if args.len() <= 1 {
        return false;
    }

    if args[1] == "help" {
        print_ui_help(args.get(2).map(String::as_str));
        return true;
    }

    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
    {
        let target = args
            .get(1)
            .filter(|arg| arg.as_str() != "-h" && arg.as_str() != "--help")
            .map(String::as_str);
        print_ui_help(target);
        return true;
    }

    false
}

fn print_ui_help(target: Option<&str>) {
    if !zh() {
        let mut command = Cli::command();
        let _ = if let Some(target) = target {
            if let Some(subcommand) = command.find_subcommand_mut(target) {
                subcommand.print_help()
            } else {
                command.print_help()
            }
        } else {
            command.print_help()
        };
        println!();
        return;
    }

    match target.unwrap_or("") {
        "login" => println!(
            "登录 ChatGPT 账号\n\n用法：cas login [browser|device]\n\n参数：\n  [browser|device]  登录方式；不指定时进入选择器\n\n选项：\n  -h, --help  显示帮助"
        ),
        "status" => println!(
            "刷新凭据有效性和 Codex 剩余用量\n\n用法：cas status [账号]\n\n参数：\n  [账号]  不区分大小写的邮箱前缀；多个匹配时进入账号选择器\n\n选项：\n  -h, --help  显示帮助"
        ),
        "test" | "refresh" | "test/refresh" => println!(
            "并发测试所有已保存账号\n\n用法：cas test/refresh\n\n行为：\n  使用每个已保存 auth 向 gpt-6-luna 发送一个流式、无上下文的 hello，并选择模型声明支持的最低 reasoning effort\n\n选项：\n  -h, --help  显示帮助"
        ),
        "switch" | "enable" | "switch/enable" => println!(
            "结束 Codex 并切换账号\n\n用法：cas switch/enable [账号]\n\n参数：\n  [账号]  不区分大小写的邮箱前缀；多个匹配时进入账号选择器\n\n选项：\n  -h, --help  显示帮助"
        ),
        "import" | "input" | "import/input" => println!(
            "导入一个 auth.json\n\n用法：cas import/input [路径]\n\n参数：\n  [路径]  auth.json 路径；不指定时导入当前 ~/.codex/auth.json\n\n选项：\n  -h, --help  显示帮助"
        ),
        "remove" | "delete" | "remove/delete" => println!(
            "删除一个已保存账号\n\n用法：cas remove/delete [完整邮箱]\n\n参数：\n  [完整邮箱]  完整邮箱地址；同邮箱多个工作区时进入账号选择器\n\n选项：\n  -h, --help  显示帮助"
        ),
        _ => println!(
            "Codex 的 ChatGPT 账号切换器\n\n用法：cas [命令]\n\n命令：\n  login          登录 ChatGPT 账号\n  status         刷新凭据有效性和 Codex 剩余用量\n  test/refresh   并发测试所有账号的 gpt-6-luna 流式响应\n  switch/enable  结束 Codex 并切换账号\n  import/input   导入 auth.json\n  remove/delete  删除已保存账号\n  help           显示帮助\n\n选项：\n  -h, --help     显示帮助\n  -V, --version  显示版本"
        ),
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn select_matching_account(
    cas: &Cas,
    prompt: &str,
    accounts: Vec<AccountChoice>,
    selector: &str,
) -> cas_core::Result<Option<AccountChoice>> {
    match accounts.len() {
        0 => Err(CasError::AccountNotFound(selector.into())),
        1 => Ok(accounts.into_iter().next()),
        _ => select_account(cas, prompt, &accounts),
    }
}

fn select_account(
    cas: &Cas,
    prompt: &str,
    accounts: &[AccountChoice],
) -> cas_core::Result<Option<AccountChoice>> {
    let accounts = refresh_account_choices(cas, accounts)?;
    let labels: Vec<_> = accounts.iter().map(format_account_choice).collect();
    Ok(select_menu(prompt, &labels, true)?.map(|index| accounts[index].clone()))
}

fn refresh_account_choices(
    cas: &Cas,
    accounts: &[AccountChoice],
) -> cas_core::Result<Vec<AccountChoice>> {
    let ids: Vec<_> = accounts
        .iter()
        .map(|account| account.account.id.clone())
        .collect();
    let _ = cas.status_ids(&ids)?;

    let refreshed = cas.account_choices()?;
    let by_id: std::collections::HashMap<_, _> = refreshed
        .into_iter()
        .map(|choice| (choice.account.id.clone(), choice))
        .collect();
    accounts
        .iter()
        .map(|choice| {
            by_id.get(&choice.account.id).cloned().ok_or_else(|| {
                CasError::Verification(format!(
                    "account disappeared while refreshing selector: {}",
                    choice.account.id
                ))
            })
        })
        .collect()
}

fn format_account_choice(choice: &AccountChoice) -> String {
    let account = &choice.account;
    let email = account.email.as_deref().unwrap_or(if zh() {
        "<未知邮箱>"
    } else {
        "<unknown-email>"
    });
    let auth_type = choice
        .auth_type
        .as_deref()
        .unwrap_or(if zh() { "未知" } else { "unknown" });
    let refresh = choice
        .token_last_refresh_millis
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| {
            if zh() {
                "未知".into()
            } else {
                "unknown".into()
            }
        });
    let workspace =
        account
            .account_id
            .as_deref()
            .unwrap_or(if zh() { "未知" } else { "unknown" });
    let alias = account
        .alias
        .as_deref()
        .filter(|alias| !alias.trim().is_empty())
        .map(|alias| {
            let key = if zh() { "别名" } else { "alias" };
            format!(
                "{}{}{}{}",
                label(key),
                dim("="),
                paint(alias, Color::Yellow),
                separator()
            )
        })
        .unwrap_or_default();
    let usage = account
        .last_status
        .as_ref()
        .map(|status| {
            let five = status
                .five_hour
                .as_ref()
                .map(|window| format_usage_window(if zh() { "5小时" } else { "5h" }, window))
                .unwrap_or_else(|| unavailable_usage(if zh() { "5小时" } else { "5h" }));
            let long = status
                .long_window
                .as_ref()
                .map(|window| {
                    let label = if zh() {
                        match window.label() {
                            "week" => "周",
                            "30d" => "30天",
                            _ => "长期",
                        }
                    } else {
                        window.label()
                    };
                    format_usage_window(label, window)
                })
                .unwrap_or_else(|| unavailable_usage(if zh() { "周/30天" } else { "week/30d" }));
            format!("{five}{}{long}", separator())
        })
        .unwrap_or_else(|| {
            format!(
                "{}{}{}",
                unavailable_usage(if zh() { "5小时" } else { "5h" }),
                separator(),
                unavailable_usage(if zh() { "周/30天" } else { "week/30d" })
            )
        });
    let email = paint(email, Color::White);
    let auth_type = paint(auth_type, Color::Blue);
    let refresh = dim(refresh);
    let workspace = paint(workspace, Color::Magenta);
    let refresh_field = format!(
        "{}{}{}",
        label(if zh() { "凭据" } else { "token" }),
        dim("="),
        refresh
    );
    let workspace_field = format!(
        "{}{}{}",
        label(if zh() { "工作区" } else { "workspace" }),
        dim("="),
        workspace
    );
    format!(
        "{alias}{email}{}{auth_type}{}{usage}{}{refresh_field}{}{workspace_field}",
        separator(),
        separator(),
        separator(),
        separator(),
    )
}

fn select_login_method() -> cas_core::Result<Option<LoginMethod>> {
    let labels = if zh() {
        ["浏览器".to_owned(), "设备码".to_owned()]
    } else {
        ["Browser".to_owned(), "Device code".to_owned()]
    };
    let prompt = if zh() { "登录方式" } else { "Login method" };
    Ok(match select_menu(prompt, &labels, true)? {
        Some(0) => Some(LoginMethod::Browser),
        Some(1) => Some(LoginMethod::Device),
        Some(_) => unreachable!(),
        None => None,
    })
}

fn select_main_action() -> cas_core::Result<Option<MainAction>> {
    const ACTIONS: &[(MainAction, &str)] = &[
        (MainAction::Status, "status"),
        (MainAction::TestRefresh, "test/refresh"),
        (MainAction::Switch, "switch/enable"),
        (MainAction::Login, "login"),
        (MainAction::Input, "import/input"),
        (MainAction::Remove, "remove/delete"),
        (MainAction::Help, "help"),
    ];
    let labels: Vec<_> = ACTIONS
        .iter()
        .map(|(_, label)| (*label).to_owned())
        .collect();
    Ok(select_menu("CAS", &labels, false)?.map(|index| ACTIONS[index].0))
}

fn select_menu(
    prompt: &str,
    labels: &[String],
    with_exit: bool,
) -> cas_core::Result<Option<usize>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CasError::Verification(if zh() {
            "交互菜单需要终端".into()
        } else {
            "interactive menu requires a terminal".into()
        }));
    }
    if labels.is_empty() {
        return Ok(None);
    }

    let _raw = RawModeGuard::enable()?;
    let mut stdout = io::stdout().lock();
    let mut selected = 0usize;
    let mut displayed_rows = 0u16;
    let item_count = labels.len() + usize::from(with_exit);

    loop {
        if displayed_rows > 0 {
            queue!(
                stdout,
                cursor::MoveUp(displayed_rows),
                cursor::MoveToColumn(0),
                Clear(ClearType::FromCursorDown)
            )?;
        }
        let columns = terminal::size()?.0.max(1);
        displayed_rows = menu_rendered_rows(prompt, labels, columns, with_exit);
        queue!(stdout, Print(prompt), Print("\r\n"))?;
        for index in 0..item_count {
            let label = if with_exit && index == 0 {
                if zh() { "不做更改" } else { "No changes" }
            } else {
                &labels[index - usize::from(with_exit)]
            };
            if index == selected {
                queue!(
                    stdout,
                    SetAttribute(Attribute::Reverse),
                    Print(format!("> {label}")),
                    SetAttribute(Attribute::Reset),
                    Print("\r\n")
                )?;
            } else {
                queue!(stdout, Print(format!("  {label}\r\n")))?;
            }
        }
        stdout.flush()?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if is_cancel_key(key) {
            clear_selector(&mut stdout, displayed_rows)?;
            return Ok(None);
        }
        match key.code {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(item_count - 1),
            KeyCode::Enter => {
                clear_selector(&mut stdout, displayed_rows)?;
                return Ok(if with_exit {
                    selected.checked_sub(1)
                } else {
                    Some(selected)
                });
            }
            _ => {}
        }
    }
}
fn menu_rendered_rows(prompt: &str, labels: &[String], columns: u16, with_exit: bool) -> u16 {
    let columns = usize::from(columns.max(1));
    let mut rows = wrapped_rows(prompt, columns);
    if with_exit {
        rows = rows.saturating_add(wrapped_rows(
            if zh() {
                "> 不做更改"
            } else {
                "> No changes"
            },
            columns,
        ));
    }
    for label in labels {
        rows = rows.saturating_add(wrapped_rows(&format!("  {label}"), columns));
    }
    rows.min(usize::from(u16::MAX)) as u16
}

fn wrapped_rows(text: &str, columns: usize) -> usize {
    visible_width(text).max(1).div_ceil(columns.max(1))
}

fn visible_width(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut plain = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
            continue;
        }
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        plain.push(ch);
        index += ch.len_utf8();
    }
    UnicodeWidthStr::width(plain.as_str())
}

fn is_cancel_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C')))
}

fn clear_selector(stdout: &mut impl Write, displayed_rows: u16) -> io::Result<()> {
    queue!(
        stdout,
        cursor::MoveUp(displayed_rows),
        cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    )?;
    stdout.flush()
}

fn format_status(status: &AccountStatus) -> String {
    let email = status.account.email.as_deref().unwrap_or(if zh() {
        "<未知邮箱>"
    } else {
        "<unknown-email>"
    });
    let workspace =
        status
            .account
            .account_id
            .as_deref()
            .unwrap_or(if zh() { "未知" } else { "unknown" });
    if zh() {
        let mut parts = vec![
            format!("{}{}", label("状态："), paint(email, Color::White)),
            field("工作区", workspace, Color::Magenta),
            match status.snapshot.valid {
                Some(true) => field("有效", "是", Color::Green),
                Some(false) => field("有效", "否", Color::Red),
                None => field("有效", "未知", Color::Yellow),
            },
        ];

        if let Some(plan) = status.snapshot.plan_type.as_deref() {
            parts.push(field("套餐", plan, Color::Blue));
        }
        if let Some(window) = status.snapshot.five_hour.as_ref() {
            parts.push(format_usage_window("5小时", window));
        }
        if let Some(window) = status.snapshot.long_window.as_ref() {
            let label = match window.label() {
                "week" => "周",
                "30d" => "30天",
                _ => "长期",
            };
            parts.push(format_usage_window(label, window));
        } else {
            parts.push(unavailable_usage("周/30天"));
        }
        if let Some(error) = status.snapshot.error.as_deref() {
            parts.push(field("错误", error, Color::Red));
        }
        return parts.join(&separator());
    }

    let mut parts = vec![
        format!("{}{}", label("STATUS: "), paint(email, Color::White)),
        field("workspace", workspace, Color::Magenta),
        match status.snapshot.valid {
            Some(true) => field("valid", "yes", Color::Green),
            Some(false) => field("valid", "no", Color::Red),
            None => field("valid", "unknown", Color::Yellow),
        },
    ];

    if let Some(plan) = status.snapshot.plan_type.as_deref() {
        parts.push(field("plan", plan, Color::Blue));
    }
    if let Some(window) = status.snapshot.five_hour.as_ref() {
        parts.push(format_usage_window("5h", window));
    }
    if let Some(window) = status.snapshot.long_window.as_ref() {
        parts.push(format_usage_window(window.label(), window));
    } else {
        parts.push(unavailable_usage("week/30d"));
    }
    if let Some(error) = status.snapshot.error.as_deref() {
        parts.push(field("error", error, Color::Red));
    }
    parts.join(&separator())
}

fn format_usage_window(label_text: &str, window: &UsageWindow) -> String {
    let reset = window
        .resets_at
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%m-%d %H:%M")
                .to_string()
        });
    match (zh(), reset) {
        (true, Some(reset)) => {
            format!(
                "{}{}{}{}",
                label(label_text),
                dim("="),
                usage_value(window.remaining_percent),
                dim(format!("（刷新 {reset}）"))
            )
        }
        (false, Some(reset)) => {
            format!(
                "{}{}{}{}",
                label(label_text),
                dim("="),
                usage_value(window.remaining_percent),
                dim(format!(" (resets {reset})"))
            )
        }
        _ => format!(
            "{}{}{}",
            label(label_text),
            dim("="),
            usage_value(window.remaining_percent)
        ),
    }
}

fn format_current_auth(current: &cas_core::CurrentAccount) -> String {
    let Some(identity) = current.identity.as_ref() else {
        return if zh() {
            "当前 auth：无".into()
        } else {
            "active auth: none".into()
        };
    };

    let name = current
        .account
        .as_ref()
        .map(|account| account.display_name())
        .unwrap_or_else(|| identity.display_name());
    let workspace = identity
        .account_id
        .as_deref()
        .or_else(|| {
            current
                .account
                .as_ref()
                .and_then(|account| account.account_id.as_deref())
        })
        .unwrap_or(if zh() { "未知" } else { "unknown" });

    if zh() {
        let unmanaged = if current.managed {
            ""
        } else {
            "（未纳入 CAS）"
        };
        format!(
            "{}{}{}{}{}{}",
            label("当前 auth："),
            paint(name, Color::White),
            dim(" ["),
            field("工作区", workspace, Color::Magenta),
            dim("]"),
            if unmanaged.is_empty() {
                String::new()
            } else {
                paint(unmanaged, Color::Yellow)
            }
        )
    } else {
        let unmanaged = if current.managed {
            ""
        } else {
            " (not managed by CAS)"
        };
        format!(
            "{}{}{}{}{}{}",
            label("active auth: "),
            paint(name, Color::White),
            dim(" ["),
            field("workspace", workspace, Color::Magenta),
            dim("]"),
            if unmanaged.is_empty() {
                String::new()
            } else {
                paint(unmanaged, Color::Yellow)
            }
        )
    }
}

fn print_test_result(result: &cas_core::AccountTestResult) {
    let name = result.account.display_name();
    let workspace =
        result
            .account
            .account_id
            .as_deref()
            .unwrap_or(if zh() { "未知" } else { "unknown" });
    if let Some(response) = result.response.as_deref() {
        if zh() {
            println!(
                "{}{}{}{}{}{}{}{}",
                label("测试："),
                paint(&name, Color::White),
                separator(),
                field("工作区", workspace, Color::Magenta),
                separator(),
                field("模型", "gpt-6-luna", Color::Blue),
                separator(),
                field("effort", &result.reasoning_effort, Color::Yellow),
            );
            println!("{}{}", label("响应："), response);
        } else {
            println!(
                "{}{}{}{}{}{}{}{}",
                label("TEST: "),
                paint(&name, Color::White),
                separator(),
                field("workspace", workspace, Color::Magenta),
                separator(),
                field("model", "gpt-6-luna", Color::Blue),
                separator(),
                field("effort", &result.reasoning_effort, Color::Yellow),
            );
            println!("{}{}", label("response: "), response);
        }
        return;
    }

    let error = result.error.as_deref().unwrap_or(if zh() {
        "未知错误"
    } else {
        "unknown error"
    });
    if zh() {
        println!(
            "{}{}{}{}{}{}{}{}",
            label("测试："),
            paint(&name, Color::White),
            separator(),
            field("工作区", workspace, Color::Magenta),
            separator(),
            field("模型", "gpt-6-luna", Color::Blue),
            separator(),
            field("effort", &result.reasoning_effort, Color::Yellow),
        );
        println!("{}{}", label("失败："), paint(error, Color::Red));
        println!("{}", label("请求头："));
    } else {
        println!(
            "{}{}{}{}{}{}{}{}",
            label("TEST: "),
            paint(&name, Color::White),
            separator(),
            field("workspace", workspace, Color::Magenta),
            separator(),
            field("model", "gpt-6-luna", Color::Blue),
            separator(),
            field("effort", &result.reasoning_effort, Color::Yellow),
        );
        println!("{}{}", label("failed: "), paint(error, Color::Red));
        println!("{}", label("request headers:"));
    }
    if result.request_headers.is_empty() {
        println!(
            "  {}",
            if zh() {
                "（请求未发出）"
            } else {
                "(request was not sent)"
            }
        );
    } else {
        for (name, value) in &result.request_headers {
            println!("  {name}: {value}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Command, format_account_choice, menu_rendered_rows, usage_color, visible_width, zh,
    };
    use cas_core::{AccountChoice, AccountRecord, AccountStatusSnapshot, UsageWindow};
    use clap::Parser;
    use crossterm::style::Color;

    #[test]
    fn usage_gradient_runs_red_through_yellow_to_green() {
        assert_eq!(usage_color(0), Color::Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(
            usage_color(50),
            Color::Rgb {
                r: 255,
                g: 255,
                b: 0
            }
        );
        assert_eq!(usage_color(100), Color::Rgb { r: 0, g: 255, b: 0 });
    }

    #[test]
    fn ansi_colors_do_not_change_terminal_width() {
        assert_eq!(visible_width("\x1b[38;2;255;0;0m50%\x1b[39m"), 3);
        assert_eq!(visible_width("周=\x1b[38;2;0;255;0m100%\x1b[39m"), 7);
    }

    #[test]
    fn selector_counts_wrapped_terminal_rows() {
        let labels = vec!["1234567890".to_owned(), "abc".to_owned()];
        assert_eq!(menu_rendered_rows("Menu", &labels, 8, true), 6);
        assert_eq!(menu_rendered_rows("Menu", &labels, 80, true), 4);
        assert_eq!(menu_rendered_rows("Menu", &labels, 8, false), 4);
        assert_eq!(menu_rendered_rows("Menu", &labels, 80, false), 3);
    }

    #[test]
    fn account_choice_includes_workspace_plan_and_refresh() {
        let choice = AccountChoice {
            account: AccountRecord {
                id: "00000000-0000-0000-0000-000000000001".into(),
                alias: None,
                email: Some("same@example.com".into()),
                account_id: Some("workspace-business".into()),
                user_id: None,
                created_at: 0,
                updated_at: 0,
                last_activated_at: None,
                last_status: Some(AccountStatusSnapshot {
                    checked_at: 0,
                    valid: Some(true),
                    plan_type: Some("team".into()),
                    five_hour: Some(UsageWindow {
                        used_percent: 12,
                        remaining_percent: 88,
                        resets_at: None,
                        window_duration_mins: Some(300),
                    }),
                    long_window: Some(UsageWindow {
                        used_percent: 47,
                        remaining_percent: 53,
                        resets_at: None,
                        window_duration_mins: Some(10_080),
                    }),
                    error: None,
                }),
            },
            token_last_refresh_millis: Some(1_758_844_800_000),
            auth_type: Some("business".into()),
        };
        let label = format_account_choice(&choice);
        assert!(label.contains("same@example.com"));
        assert!(label.contains("business"));
        if zh() {
            assert!(label.contains("5小时=88%"));
            assert!(label.contains("周=53%"));
            assert!(label.contains("工作区=workspace-business"));
            assert!(label.contains("凭据=2025-09-26"));
        } else {
            assert!(label.contains("5h=88%"));
            assert!(label.contains("week=53%"));
            assert!(label.contains("workspace=workspace-business"));
            assert!(label.contains("token=2025-09-26"));
        }
    }

    #[test]
    fn delete_is_an_alias_of_remove() {
        let cli = Cli::try_parse_from(["cas", "delete", "user@example.com"]).unwrap();
        match cli.command {
            Some(Command::Remove { email }) => {
                assert_eq!(email.as_deref(), Some("user@example.com"));
            }
            _ => panic!("delete did not parse as remove"),
        }
    }

    #[test]
    fn remove_still_parses_as_remove() {
        let cli = Cli::try_parse_from(["cas", "remove", "user@example.com"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Remove { .. })));
    }

    #[test]
    fn test_refresh_aliases_are_equivalent() {
        for command in ["test/refresh", "test", "refresh"] {
            let cli = Cli::try_parse_from(["cas", command]).unwrap();
            assert!(matches!(cli.command, Some(Command::TestRefresh)));
        }
    }

    #[test]
    fn enable_is_an_alias_of_switch() {
        let cli = Cli::try_parse_from(["cas", "enable", "user@example.com"]).unwrap();
        match cli.command {
            Some(Command::Switch { account }) => {
                assert_eq!(account.as_deref(), Some("user@example.com"));
            }
            _ => panic!("enable did not parse as switch"),
        }
    }

    #[test]
    fn switch_still_parses_as_switch() {
        let cli = Cli::try_parse_from(["cas", "switch", "user@example.com"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Switch { .. })));
    }

    #[test]
    fn import_and_input_are_equivalent() {
        for command in ["import", "input"] {
            let cli = Cli::try_parse_from(["cas", command, "/tmp/auth.json"]).unwrap();
            match cli.command {
                Some(Command::Input { path }) => {
                    assert_eq!(
                        path.as_deref(),
                        Some(std::path::Path::new("/tmp/auth.json"))
                    );
                }
                _ => panic!("{command} did not parse as input"),
            }
        }
    }

    #[test]
    fn bare_cas_parses_without_a_subcommand() {
        let cli = Cli::try_parse_from(["cas"]).unwrap();
        assert!(cli.command.is_none());
    }
}
