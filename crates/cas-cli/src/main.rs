mod usage_range;

use cas_core::{
    AccountChoice, AccountStatus, Cas, CasError, CasPaths, CurrentAccount, UsageReport,
    UsageWindow, scan_codex_usage_in_range,
};
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

fn count_field(label_text: &str, value: impl std::fmt::Display, color: Color) -> String {
    field(label_text, &value.to_string(), color)
}

fn usd_field(label_text: &str, usd: &str) -> String {
    field(label_text, &format!("${usd}"), Color::Green)
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
    /// Remove one saved account; active auth requires confirmation and Codex shutdown.
    #[command(name = "remove/delete", aliases = ["remove", "delete"])]
    Remove {
        /// Complete email address. Multiple account matches open the account selector.
        email: Option<String>,
    },
    /// Estimate Codex JSONL usage with an interactive date filter on terminals.
    #[command(name = "usage/price", aliases = ["usage", "price"])]
    Usage {
        /// Optional JSONL file or directory; defaults to CODEX_HOME sessions and archives.
        path: Option<PathBuf>,
        /// Print structured statistics, including per-response data, as JSON.
        #[arg(long)]
        json: bool,
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
    Usage,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountSelectionMode {
    General,
    Switch,
    Remove,
}

fn main() {
    if maybe_print_localized_help() {
        return;
    }
    let cli = Cli::parse();
    // Local JSONL usage must work even when HTTP client initialization is
    // unavailable. The existing network commands retain their startup check.
    if !matches!(cli.command, Some(Command::Usage { .. }))
        && let Err(error) = cas_core::initialize_http_client()
    {
        if zh() {
            eprintln!("错误：HTTP 客户端初始化失败：{error}");
        } else {
            eprintln!("ERROR: HTTP client initialization failed: {error}");
        }
        std::process::exit(1);
    }
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
                MainAction::Usage => Command::Usage {
                    path: None,
                    json: false,
                },
                MainAction::Help => {
                    print_ui_help(None);
                    return Ok(());
                }
            }
        }
    };

    // Usage is intentionally self-contained and read-only: do not require an
    // auth.json, create CAS state directories, or contact the Codex service.
    if let Command::Usage { path, json } = &command {
        let paths = CasPaths::discover()?;
        // Scripts and --json retain the previous noninteractive all-time
        // behavior. On a terminal the selection UI runs before scanning.
        let time_range = if !*json && io::stdin().is_terminal() && io::stdout().is_terminal() {
            match usage_range::choose_usage_range()? {
                Some(usage_range::UsageRangeChoice::Bounded(range)) => Some(range),
                Some(usage_range::UsageRangeChoice::All) => None,
                None => return Ok(()),
            }
        } else {
            None
        };
        let report = scan_codex_usage_in_range(&paths, path.as_deref(), time_range)?;
        if *json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_local_usage(&report);
        }
        return Ok(());
    }

    let cas = Cas::discover()?;
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
                    "已保存 {} [账号ID={}]。当前 Codex 账号未更改。",
                    account.display_name(),
                    workspace
                );
            } else {
                println!(
                    "saved {} [account ID={}]. Active Codex account was not changed.",
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
                    let Some(account) = select_matching_account(
                        &cas,
                        prompt,
                        matches,
                        &selector,
                        None,
                        AccountSelectionMode::General,
                    )?
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
            let current = cas.current_account()?;
            println!("{}", format_current_auth(&current));
            let account = match account.as_deref() {
                Some(selector) => {
                    let matches = cas.account_choices_prefix(selector)?;
                    let prompt = if zh() {
                        "选择要切换的账号"
                    } else {
                        "Switch account"
                    };
                    let Some(account) = select_matching_account(
                        &cas,
                        prompt,
                        matches,
                        selector,
                        Some(&current),
                        AccountSelectionMode::Switch,
                    )?
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
                    let Some(account) = select_account(
                        &cas,
                        prompt,
                        &choices,
                        Some(&current),
                        AccountSelectionMode::Switch,
                    )?
                    else {
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
                    "已切换到 {} [账号ID={}]；退出了 {} 个进程。",
                    target_name,
                    workspace,
                    result.terminated_processes.len(),
                );
            } else {
                println!(
                    "switched to {} [account ID={}]; exited {} processes.",
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
            let current = cas.current_account()?;
            println!("{}", format_current_auth(&current));
            let account = match email.as_deref() {
                Some(email) => {
                    let matches = cas.account_choices_exact_email(email)?;
                    let prompt = if zh() {
                        "选择要删除的账号"
                    } else {
                        "Remove account"
                    };
                    let Some(account) = select_matching_account(
                        &cas,
                        prompt,
                        matches,
                        email,
                        Some(&current),
                        AccountSelectionMode::Remove,
                    )?
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
                    let Some(account) = select_account(
                        &cas,
                        prompt,
                        &choices,
                        Some(&current),
                        AccountSelectionMode::Remove,
                    )?
                    else {
                        return Ok(());
                    };
                    account
                }
            };
            // An active auth is visually locked and requires an explicit
            // second confirmation. Ordinary deletions must never stop Codex.
            let active_removal = is_selected_active(&account, &current);
            let (removed, terminated_processes) = if active_removal {
                if !confirm_active_removal(&account)? {
                    return Ok(());
                }
                let result = cas.remove_active_id(&account.account.id)?;
                (result.account, Some(result.terminated_processes))
            } else {
                (cas.remove_id(&account.account.id)?, None)
            };
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
                println!("已删除 {removed_name} [账号ID={workspace}]。");
                if let Some(processes) = terminated_processes {
                    println!(
                        "已关闭 {} 个 Codex 进程；已移除当前 auth.json，Codex 现处于未登录状态。",
                        processes.len()
                    );
                }
            } else {
                println!("removed {removed_name} [account ID={workspace}].");
                if let Some(processes) = terminated_processes {
                    println!(
                        "terminated {} Codex process(es); active auth.json removed; Codex is signed out.",
                        processes.len()
                    );
                }
            }
        }
        Command::Usage { .. } => unreachable!("usage handled before opening CAS state"),
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
            Self::Usage { .. } => "usage/price",
        }
    }
}

fn print_local_usage(report: &UsageReport) {
    let chinese = zh();
    println!(
        "{}",
        label(if chinese {
            "Codex 本地 JSONL 用量统计（不联网、只读）"
        } else {
            "Codex local JSONL usage (offline, read-only)"
        })
    );
    println!(
        "{}",
        label(if chinese {
            "扫描目录："
        } else {
            "Scan roots:"
        })
    );
    for root in &report.scan_roots {
        println!("  {}", paint(root.display().to_string(), Color::White));
    }
    if let Some(range) = report.time_range.as_ref() {
        let format_time = |timestamp: &DateTime<Utc>| {
            timestamp
                .with_timezone(&Local)
                .format("%Y/%m/%d %H:%M:%S %:z")
                .to_string()
        };
        println!(
            "{}",
            [
                field(
                    if chinese { "起始" } else { "from" },
                    &format_time(&range.start),
                    Color::Green
                ),
                field(
                    if chinese { "终止" } else { "until" },
                    &format_time(&range.end),
                    Color::Green
                ),
            ]
            .join(&separator())
        );
        println!(
            "{}",
            [
                count_field(
                    if chinese {
                        "区间外响应"
                    } else {
                        "outside range"
                    },
                    report.excluded_outside_range,
                    Color::White
                ),
                count_field(
                    if chinese {
                        "时间缺失已跳过"
                    } else {
                        "untimed excluded"
                    },
                    report.excluded_without_timestamp,
                    if report.excluded_without_timestamp == 0 {
                        Color::Green
                    } else {
                        Color::Yellow
                    }
                ),
            ]
            .join(&separator())
        );
    }
    if report.files_scanned == 0 {
        println!(
            "{}",
            paint(
                if chinese {
                    "未发现 JSONL 会话文件；请检查 CODEX_HOME 或通过 cas usage [路径] 指定位置。"
                } else {
                    "No session JSONL files found; check CODEX_HOME or pass a path to cas usage."
                },
                Color::Yellow
            )
        );
        return;
    }

    let counts = &report.totals;
    println!(
        "{}",
        [
            count_field(
                if chinese { "文件" } else { "files" },
                report.files_scanned,
                Color::White
            ),
            count_field(
                if chinese { "有用量" } else { "with usage" },
                report.files_with_usage,
                Color::Green
            ),
            count_field(
                if chinese { "响应" } else { "responses" },
                report.responses,
                Color::Green
            ),
            count_field(
                if chinese {
                    "跨文件重复"
                } else {
                    "duplicates"
                },
                report.duplicate_responses,
                if report.duplicate_responses == 0 {
                    Color::Green
                } else {
                    Color::Yellow
                }
            ),
        ]
        .join(&separator())
    );
    println!(
        "{}",
        [
            count_field(
                if chinese { "输入" } else { "input" },
                counts.input_tokens,
                Color::White
            ),
            count_field(
                if chinese { "新增" } else { "fresh" },
                counts.fresh_input_tokens(),
                Color::Yellow
            ),
            count_field(
                if chinese {
                    "缓存读取"
                } else {
                    "cache read"
                },
                counts.cached_input_tokens,
                Color::Magenta
            ),
            count_field(
                if chinese {
                    "缓存写入"
                } else {
                    "cache write"
                },
                counts.cache_write_input_tokens,
                Color::Blue
            ),
        ]
        .join(&separator())
    );
    println!(
        "{}",
        [
            count_field(
                if chinese { "输出" } else { "output" },
                counts.output_tokens,
                Color::White
            ),
            count_field(
                if chinese { "推理" } else { "reasoning" },
                counts.reasoning_output_tokens,
                Color::Magenta
            ),
            count_field("Token", counts.total_tokens(), Color::Green),
        ]
        .join(&separator())
    );
    println!("{}", label(if chinese { "按模型：" } else { "By model:" }));
    for item in &report.models {
        let parts = [
            count_field(
                if chinese { "响应" } else { "responses" },
                item.responses,
                Color::Green,
            ),
            count_field(
                if chinese { "输入" } else { "input" },
                item.tokens.input_tokens,
                Color::White,
            ),
            count_field(
                if chinese {
                    "缓存读取"
                } else {
                    "cache read"
                },
                item.tokens.cached_input_tokens,
                Color::Magenta,
            ),
            count_field(
                if chinese {
                    "缓存写入"
                } else {
                    "cache write"
                },
                item.tokens.cache_write_input_tokens,
                Color::Blue,
            ),
            count_field(
                if chinese { "输出" } else { "output" },
                item.tokens.output_tokens,
                Color::White,
            ),
        ];
        println!(
            "  {}{}{}",
            paint(&item.model, Color::Magenta),
            dim(if chinese { "：" } else { ": " }),
            parts.join(&separator())
        );
    }
    println!(
        "{}",
        label(if chinese {
            "请求 tier（日志配置值，非服务端实际执行值）："
        } else {
            "Requested tier (logged preference, not server-confirmed):"
        })
    );
    for tier in &report.tiers {
        let value = tier
            .requested_service_tier
            .as_deref()
            .unwrap_or(if chinese { "未知" } else { "unknown" });
        let tier_color = match tier.requested_service_tier.as_deref() {
            Some("default" | "standard") => Color::Green,
            Some("priority" | "fast" | "ultrafast") => Color::Yellow,
            Some("flex") => Color::Blue,
            _ => Color::Yellow,
        };
        let parts = [
            count_field(
                if chinese { "响应" } else { "responses" },
                tier.responses,
                Color::Green,
            ),
            count_field(
                if chinese { "输入" } else { "input" },
                tier.tokens.input_tokens,
                Color::White,
            ),
            count_field(
                if chinese { "输出" } else { "output" },
                tier.tokens.output_tokens,
                Color::White,
            ),
        ];
        println!(
            "  {}{}{}",
            paint(value, tier_color),
            dim(if chinese { "：" } else { ": " }),
            parts.join(&separator())
        );
    }
    let price = &report.pricing;
    println!(
        "{}",
        label(if chinese {
            "OpenAI API 标准费率参考（USD，仅文本 Token）："
        } else {
            "OpenAI API Standard rate reference (USD, text tokens only):"
        })
    );
    println!(
        "  {}",
        [
            usd_field(
                if chinese { "标准费用" } else { "Standard" },
                &price.standard_usd
            ),
            field(
                if chinese {
                    "费率日期"
                } else {
                    "rates as of"
                },
                &price.as_of,
                Color::Blue
            ),
            field(
                if chinese { "已计价" } else { "priced" },
                &format!("{} / {}", price.priced_responses, report.responses),
                Color::Green
            ),
            count_field(
                if chinese {
                    "长上下文加价"
                } else {
                    "long context"
                },
                price.long_context_responses,
                Color::Yellow
            ),
        ]
        .join(&separator())
    );
    println!(
        "{}",
        label(if chinese {
            "按精确模型 ID 计价："
        } else {
            "Pricing by exact model ID:"
        })
    );
    for item in &price.models {
        println!(
            "  {}{}{}",
            paint(&item.model, Color::Magenta),
            dim(if chinese { "：" } else { ": " }),
            [
                count_field(
                    if chinese { "响应" } else { "responses" },
                    item.responses,
                    Color::Green
                ),
                usd_field("Standard", &item.standard_usd),
            ]
            .join(&separator())
        );
    }
    if price.unpriced_responses > 0 {
        eprintln!(
            "{}{}",
            field(
                if chinese { "未计价" } else { "unpriced" },
                &price.unpriced_responses.to_string(),
                Color::Red
            ),
            dim(if chinese {
                "（费用不包含这些响应）："
            } else {
                " (excluded from USD total):"
            })
        );
        for item in &price.unpriced {
            eprintln!(
                "  {}: {} ({})",
                paint(&item.model, Color::Magenta),
                paint(item.responses.to_string(), Color::Red),
                dim(&item.reason)
            );
        }
    }
    if report.warning_count > 0 {
        eprintln!(
            "{}{}:",
            label(if chinese {
                "扫描警告"
            } else {
                "Scan warnings"
            }),
            paint(report.warning_count.to_string(), Color::Yellow)
        );
        for warning in &report.warnings {
            eprintln!("  {}", paint(warning, Color::Yellow));
        }
        if report.warning_count > report.warnings.len() {
            eprintln!(
                "  ... ({} more)",
                report.warning_count - report.warnings.len()
            );
        }
    }
    println!(
        "{}",
        dim(if chinese {
            "注：标准 API 参考价非实际账单；未对无法验证的 Fast 执行 tier 加价。"
        } else {
            "Note: Standard API reference is not actual billing; no unverified Fast-tier surcharges."
        })
    );
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
            "删除一个已保存账号\n\n用法：cas remove/delete [完整邮箱]\n\n参数：\n  [完整邮箱]  完整邮箱地址；同邮箱多个账号ID时进入账号选择器\n\n行为：\n  当前生效 auth 在列表中标为锁定，删除前需再次交互确认。\n  确认后先关闭 Codex 进程，再删除当前 auth.json 及 CAS 中保存的对应账号；其他账号不受影响。\n  非交互终端不允许删除生效账号。\n\n选项：\n  -h, --help  显示帮助"
        ),
        "usage" | "price" | "usage/price" => println!(
            "只读统计 Codex 本地 JSONL 用量及 API 等价美元费用\n\n用法：cas usage/price [路径] [--json]\n\n参数：\n  [路径]    可选 JSONL 文件或目录；默认扫描 CODEX_HOME/sessions 与 archived_sessions\n\n交互选择：\n  1d（今天）、24h、3d、7d、1m、all，或手动输入起始和终止日期。\n  手动编辑 yyyy/mm/dd/hh/mm，上下键切换，留空字段按当前时间填充。\n  --json 或重定向输出时不弹出选择页面，默认统计全部。\n\n行为：\n  仅精确匹配 GPT-5.2 至 GPT-6.1 官方模型 ID；支持缓存和长上下文。\n  按官方 2026-10-08 标准价格快照计价，不推断 Fast 实际执行 tier，非 ChatGPT 实际扣费。\n\n选项：\n  --json      输出逐响应 Token、计价和汇总 JSON\n  -h, --help  显示帮助"
        ),
        _ => println!(
            "Codex 的 ChatGPT 账号切换器\n\n用法：cas [命令]\n\n命令：\n  login          登录 ChatGPT 账号\n  status         刷新凭据有效性和 Codex 剩余用量\n  test/refresh   并发测试所有账号的 gpt-6-luna 流式响应\n  switch/enable  结束 Codex 并切换账号\n  import/input   导入 auth.json\n  remove/delete  删除已保存账号\n  usage/price    只读统计本地 JSONL Token 消耗及 API 等价美元价格\n  help           显示帮助\n\n选项：\n  -h, --help     显示帮助\n  -V, --version  显示版本"
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
    current: Option<&CurrentAccount>,
    mode: AccountSelectionMode,
) -> cas_core::Result<Option<AccountChoice>> {
    match accounts.len() {
        0 => Err(CasError::AccountNotFound(selector.into())),
        1 => Ok(accounts.into_iter().next()),
        _ => select_account(cas, prompt, &accounts, current, mode),
    }
}

fn select_account(
    cas: &Cas,
    prompt: &str,
    accounts: &[AccountChoice],
    current: Option<&CurrentAccount>,
    mode: AccountSelectionMode,
) -> cas_core::Result<Option<AccountChoice>> {
    let accounts = refresh_account_choices(cas, accounts)?;
    let labels: Vec<_> = accounts
        .iter()
        .map(|choice| format_account_choice_for_action(choice, current, mode))
        .collect();
    Ok(select_menu(prompt, &labels, true)?.map(|index| accounts[index].clone()))
}

fn is_selected_active(choice: &AccountChoice, current: &CurrentAccount) -> bool {
    current
        .account
        .as_ref()
        .is_some_and(|active| active.id == choice.account.id)
}

fn format_account_choice_for_action(
    choice: &AccountChoice,
    current: Option<&CurrentAccount>,
    mode: AccountSelectionMode,
) -> String {
    let is_active = current.is_some_and(|current| is_selected_active(choice, current));
    if !is_active || mode == AccountSelectionMode::General {
        return format_account_choice(choice);
    }
    let marker = if mode == AccountSelectionMode::Remove {
        if zh() {
            "[锁定·当前生效] "
        } else {
            "[LOCKED·ACTIVE] "
        }
    } else if zh() {
        "[当前生效] "
    } else {
        "[ACTIVE] "
    };

    let mut active_choice = choice.clone();
    if let Some(plan) = current.and_then(|current| current.plan_type.as_ref()) {
        active_choice.auth_type = Some(plan.clone());
    }
    format!(
        "{}{}",
        paint(
            marker,
            if mode == AccountSelectionMode::Remove {
                Color::Yellow
            } else {
                Color::Green
            }
        ),
        format_account_choice(&active_choice)
    )
}

fn confirm_active_removal(choice: &AccountChoice) -> cas_core::Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CasError::Verification(if zh() {
            "删除当前生效账号必须在终端交互确认，以便先关闭 Codex。".into()
        } else {
            "deleting the active auth requires interactive terminal confirmation before shutting down Codex".into()
        }));
    }
    let name = choice.account.display_name();
    println!(
        "{}",
        paint(
            if zh() {
                "警告：所选 auth 正在生效；删除将关闭所有 Codex 进程，并移除当前 auth.json。"
            } else {
                "Warning: this auth is active; deletion stops all Codex processes and removes active auth.json."
            },
            Color::Yellow
        )
    );
    let options = if zh() {
        vec![
            "取消，保留当前账号".to_owned(),
            format!("关闭 Codex 并删除 {name}（同时退出当前账号）"),
        ]
    } else {
        vec![
            "Cancel; keep active account".to_owned(),
            format!("Stop Codex and delete {name} (sign out)"),
        ]
    };
    Ok(matches!(
        select_menu(
            if zh() {
                "确认解锁并删除生效 auth"
            } else {
                "Confirm active auth deletion"
            },
            &options,
            false,
        )?,
        Some(1)
    ))
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
    let auth_type = field(if zh() { "套餐" } else { "plan" }, auth_type, Color::Blue);
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
        label(if zh() { "账号ID" } else { "account ID" }),
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
        (MainAction::Usage, "usage/price"),
        (MainAction::Help, "help"),
    ];
    let labels: Vec<_> = ACTIONS
        .iter()
        .map(|(_, label)| (*label).to_owned())
        .collect();
    Ok(select_menu("CAS", &labels, false)?.map(|index| ACTIONS[index].0))
}

/// The same wrap-around navigation is used by every keyboard selector,
/// including the manually edited usage date fields.
fn cycle_selection(selected: usize, count: usize, up: bool) -> usize {
    if count == 0 {
        return 0;
    }
    if up {
        if selected == 0 {
            count - 1
        } else {
            selected - 1
        }
    } else if selected == count - 1 {
        0
    } else {
        selected + 1
    }
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
            KeyCode::Up => selected = cycle_selection(selected, item_count, true),
            KeyCode::Down => selected = cycle_selection(selected, item_count, false),
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
            field("账号ID", workspace, Color::Magenta),
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
        field("account ID", workspace, Color::Magenta),
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
    let plan = current
        .plan_type
        .as_deref()
        .unwrap_or(if zh() { "未知" } else { "unknown" });

    if zh() {
        let unmanaged = if current.managed {
            ""
        } else {
            "（未纳入 CAS）"
        };
        format!(
            "{}{}{}{}{}{}{}{}",
            label("当前 auth："),
            paint(name, Color::White),
            dim(" ["),
            field("账号ID", workspace, Color::Magenta),
            separator(),
            field("套餐", plan, Color::Blue),
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
            "{}{}{}{}{}{}{}{}",
            label("active auth: "),
            paint(name, Color::White),
            dim(" ["),
            field("account ID", workspace, Color::Magenta),
            separator(),
            field("plan", plan, Color::Blue),
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
                field("账号ID", workspace, Color::Magenta),
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
                field("account ID", workspace, Color::Magenta),
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
            field("账号ID", workspace, Color::Magenta),
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
            field("account ID", workspace, Color::Magenta),
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
        AccountSelectionMode, Cli, Command, cycle_selection, format_account_choice,
        format_account_choice_for_action, format_current_auth, is_selected_active,
        menu_rendered_rows, usage_color, visible_width, zh,
    };
    use cas_core::{
        AccountChoice, AccountRecord, AccountStatusSnapshot, CurrentAccount, UsageWindow,
    };
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
    fn all_selectors_cycle_from_first_to_last_and_last_to_first() {
        // Includes menus with an extra "不做更改" row and short lists.
        for count in [1, 2, 3, 6, 7, 8, 25] {
            assert_eq!(cycle_selection(0, count, true), count - 1);
            assert_eq!(cycle_selection(count - 1, count, false), 0);
            for selected in 1..count {
                assert_eq!(cycle_selection(selected, count, true), selected - 1);
            }
            for selected in 0..count - 1 {
                assert_eq!(cycle_selection(selected, count, false), selected + 1);
            }
        }
        assert_eq!(cycle_selection(0, 0, true), 0);
        assert_eq!(cycle_selection(0, 0, false), 0);
    }

    #[test]
    fn account_choice_includes_account_id_plan_and_refresh() {
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
            assert!(label.contains("账号ID=workspace-business"));
            assert!(label.contains("凭据=2025-09-26"));
        } else {
            assert!(label.contains("5h=88%"));
            assert!(label.contains("week=53%"));
            assert!(label.contains("account ID=workspace-business"));
            assert!(label.contains("token=2025-09-26"));
        }
    }

    #[test]
    fn switch_and_delete_mark_only_the_actual_active_account_and_show_its_plan() {
        let choice = AccountChoice {
            account: AccountRecord {
                id: "account-business".into(),
                alias: None,
                email: Some("same@example.com".into()),
                account_id: Some("shared-workspace".into()),
                user_id: Some("user-business".into()),
                created_at: 0,
                updated_at: 0,
                last_activated_at: None,
                last_status: None,
            },
            token_last_refresh_millis: None,
            auth_type: None,
        };
        let current = CurrentAccount {
            account: Some(choice.account.clone()),
            identity: Some(choice.account.identity()),
            managed: true,
            plan_type: Some("business".into()),
        };
        let active_label = format_current_auth(&current);
        assert!(active_label.contains("same@example.com"));
        assert!(active_label.contains(if zh() { "套餐" } else { "plan" }));
        assert!(active_label.contains("business"));

        assert!(is_selected_active(&choice, &current));
        let switch_label =
            format_account_choice_for_action(&choice, Some(&current), AccountSelectionMode::Switch);
        assert!(switch_label.contains(if zh() { "[当前生效]" } else { "[ACTIVE]" }));
        assert!(switch_label.contains("business"));
        let remove_label =
            format_account_choice_for_action(&choice, Some(&current), AccountSelectionMode::Remove);
        assert!(remove_label.contains(if zh() {
            "[锁定·当前生效]"
        } else {
            "[LOCKED·ACTIVE]"
        }));
        assert!(remove_label.contains("business"));

        // A second account can share its email/workspace but differ by user.
        // Only the exact saved account ID receives the active/locked marker.
        let mut other = choice;
        other.account.id = "account-free".into();
        other.account.user_id = Some("user-free".into());
        other.auth_type = Some("free".into());
        assert!(!is_selected_active(&other, &current));
        let remove_other =
            format_account_choice_for_action(&other, Some(&current), AccountSelectionMode::Remove);
        assert!(!remove_other.contains("[锁定"));
        assert!(!remove_other.contains("[LOCKED"));
        assert!(remove_other.contains("free"));
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

    #[test]
    fn usage_price_aliases_accept_optional_path_and_json() {
        for command in ["usage", "price", "usage/price"] {
            let cli =
                Cli::try_parse_from(["cas", command, "--json", "/tmp/rollout.jsonl"]).unwrap();
            match cli.command {
                Some(Command::Usage { path, json }) => {
                    assert_eq!(path.unwrap(), std::path::Path::new("/tmp/rollout.jsonl"));
                    assert!(json);
                }
                _ => panic!("{command} did not parse as usage"),
            }
        }
    }
}
