//! Inline, keyboard-driven usage date-range picker.
//!
//! Like the account selector, this redraws only the lines it owns inside the
//! current terminal. It never clears the entire screen, hides the cursor, or
//! enters the terminal's alternate-screen buffer.

use crate::{
    RawModeGuard, clear_selector, cycle_selection, dim, is_cancel_key, menu_rendered_rows, paint,
    select_menu, wrapped_rows, zh,
};
use cas_core::{CasError, Result, UsageTimeRange};
use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, Months, NaiveDate, TimeZone, Timelike, Utc,
};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    queue,
    style::{Attribute, Color, Print, SetAttribute},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, IsTerminal, Write};

const PRESETS: [&str; 6] = ["1d", "24h", "3d", "7d", "1m", "all"];
const DATE_FIELDS: [&str; 5] = ["yyyy", "mm", "dd", "hh", "mm"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UsageRangeChoice {
    All,
    Bounded(UsageTimeRange),
}

#[derive(Default, Clone, Debug)]
struct DateEditor {
    values: [String; 5],
}

impl DateEditor {
    fn edit_digit(&mut self, index: usize, digit: char) {
        let value = &mut self.values[index];
        let max_len = if index == 0 { 4 } else { 2 };
        if value.len() == max_len {
            value.clear();
        }
        value.push(digit);
    }

    fn erase(&mut self, index: usize) {
        self.values[index].pop();
    }

    fn clear(&mut self, index: usize) {
        self.values[index].clear();
    }

    fn resolve(
        &self,
        now: DateTime<Local>,
        is_end: bool,
    ) -> std::result::Result<DateTime<Utc>, (usize, String)> {
        // Both pages share the same clock reading. Leaving them completely
        // blank therefore produces a valid zero-length interval at "now".
        if self.values.iter().all(String::is_empty) {
            return Ok(now.to_utc());
        }
        let defaults = [
            now.year() as u32,
            now.month(),
            now.day(),
            now.hour(),
            now.minute(),
        ];
        let mut values = defaults;
        for (index, text) in self.values.iter().enumerate() {
            if !text.is_empty() {
                values[index] = text
                    .parse::<u32>()
                    .map_err(|_| (index, invalid_date_message("invalid number", "数字无效")))?;
            }
        }
        let year = i32::try_from(values[0])
            .map_err(|_| (0, invalid_date_message("year out of range", "年份超出范围")))?;
        let day = NaiveDate::from_ymd_opt(year, values[1], values[2]).ok_or_else(|| {
            (
                if !(1..=12).contains(&values[1]) { 1 } else { 2 },
                invalid_date_message("invalid calendar date", "日期无效"),
            )
        })?;
        let local = day.and_hms_opt(values[3], values[4], 0).ok_or_else(|| {
            (
                if values[3] > 23 { 3 } else { 4 },
                invalid_date_message("invalid hour or minute", "小时或分钟无效"),
            )
        })?;
        match Local.from_local_datetime(&local) {
            LocalResult::Single(time) => Ok(time.to_utc()),
            LocalResult::Ambiguous(earliest, latest) => Ok(if is_end {
                earliest.max(latest).to_utc()
            } else {
                earliest.min(latest).to_utc()
            }),
            LocalResult::None => Err((
                3,
                invalid_date_message(
                    "local time does not exist due to a clock change",
                    "本地时间不存在（可能处于夏令时切换期间）",
                ),
            )),
        }
    }
}

fn invalid_date_message(en: &str, cn: &str) -> String {
    if zh() { cn } else { en }.to_owned()
}

fn preset_labels() -> Vec<String> {
    let chinese = zh();
    let descriptions = if chinese {
        [
            "今天",
            "过去24小时",
            "过去3天",
            "过去7天",
            "过去1个月",
            "全部",
        ]
    } else {
        [
            "today",
            "last 24 hours",
            "last 3 days",
            "last 7 days",
            "last month",
            "all time",
        ]
    };
    let mut labels: Vec<String> = PRESETS
        .iter()
        .zip(descriptions)
        .map(|(preset, description)| format!("{preset:<4} {description}"))
        .collect();
    labels.push(if chinese {
        "手动输入  >".to_owned()
    } else {
        "Custom dates  >".to_owned()
    });
    labels
}

pub(crate) fn choose_usage_range() -> Result<Option<UsageRangeChoice>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CasError::Verification(invalid_date_message(
            "time-range picker requires a terminal",
            "时间区间选择需要终端",
        )));
    }

    // Keep the original account-selector interaction in the current terminal.
    // Manual editing is also inline, one vertically-arranged page per date.
    let now = Local::now();
    loop {
        let prompt = if zh() {
            "选择时间区间"
        } else {
            "Select time range"
        };
        let Some(preset) = select_menu(prompt, &preset_labels(), false)? else {
            return Ok(None);
        };
        if preset < PRESETS.len() {
            return preset_range(preset, now).map(Some);
        }

        let mut start_editor = DateEditor::default();
        let mut end_editor = DateEditor::default();
        loop {
            let Some(start) = edit_date(&mut start_editor, now, false, None)? else {
                break; // Esc on the first page returns to the preset list.
            };
            let Some(end) = edit_date(&mut end_editor, now, true, Some(start))? else {
                continue; // Esc on the second page returns to the first.
            };
            return Ok(Some(UsageRangeChoice::Bounded(UsageTimeRange::new(
                start, end,
            )?)));
        }
    }
}

fn edit_date(
    editor: &mut DateEditor,
    now: DateTime<Local>,
    is_end: bool,
    earliest: Option<DateTime<Utc>>,
) -> Result<Option<DateTime<Utc>>> {
    let _raw = RawModeGuard::enable()?;
    let mut stdout = io::stdout().lock();
    let mut selected = 0usize;
    let mut displayed_rows = 0u16;
    let mut error: Option<String> = None;

    loop {
        if displayed_rows != 0 {
            // Identical to the account selector: rewind only our own lines.
            queue!(
                stdout,
                cursor::MoveUp(displayed_rows),
                cursor::MoveToColumn(0),
                Clear(ClearType::FromCursorDown)
            )?;
        }
        let title = match (zh(), is_end) {
            (true, false) => "填写起始时间 (1/2)",
            (true, true) => "填写终止时间 (2/2)",
            (false, false) => "Enter start time (1/2)",
            (false, true) => "Enter end time (2/2)",
        };
        let items = date_labels(editor, now, is_end);
        let instructions = if zh() {
            "  ↑↓ 选择  数字输入  Backspace 删除  Enter 下一项/应用  Esc 返回"
        } else {
            "  Up/Down select  Digits type  Backspace erase  Enter next/apply  Esc back"
        };
        let columns = terminal::size()?.0.max(1);
        displayed_rows = menu_rendered_rows(title, &items, columns, false)
            .saturating_add(wrapped_rows(instructions, columns.into()) as u16);
        if let Some(message) = error.as_deref() {
            displayed_rows = displayed_rows
                .saturating_add(wrapped_rows(&format!("  {message}"), columns.into()) as u16);
        }

        queue!(stdout, Print(title), Print("\r\n"))?;
        for (index, item) in items.iter().enumerate() {
            if index == selected {
                queue!(
                    stdout,
                    SetAttribute(Attribute::Reverse),
                    Print(format!("> {item}")),
                    SetAttribute(Attribute::Reset),
                    Print("\r\n")
                )?;
            } else {
                queue!(stdout, Print(format!("  {item}\r\n")))?;
            }
        }
        queue!(stdout, Print(dim(instructions)), Print("\r\n"))?;
        if let Some(message) = error.as_deref() {
            queue!(
                stdout,
                Print("  "),
                Print(paint(message, Color::Red)),
                Print("\r\n")
            )?;
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
            KeyCode::Up | KeyCode::BackTab => {
                selected = cycle_selection(selected, items.len(), true);
                error = None;
            }
            KeyCode::Down | KeyCode::Tab => {
                selected = cycle_selection(selected, items.len(), false);
                error = None;
            }
            KeyCode::Backspace if selected < DATE_FIELDS.len() => {
                editor.erase(selected);
                error = None;
            }
            KeyCode::Delete if selected < DATE_FIELDS.len() => {
                editor.clear(selected);
                error = None;
            }
            KeyCode::Char(digit) if digit.is_ascii_digit() && selected < DATE_FIELDS.len() => {
                editor.edit_digit(selected, digit);
                error = None;
            }
            KeyCode::Enter if selected < DATE_FIELDS.len() - 1 => {
                selected += 1;
                error = None;
            }
            KeyCode::Enter => match editor.resolve(now, is_end) {
                Ok(resolved) if earliest.is_some_and(|start| resolved < start) => {
                    selected = DATE_FIELDS.len();
                    error = Some(invalid_date_message(
                        "end must be after start; Esc to edit the start time",
                        "终止时间不能早于起始时间；按 Esc 返回修改起始时间",
                    ));
                }
                Ok(resolved) => {
                    clear_selector(&mut stdout, displayed_rows)?;
                    return Ok(Some(resolved));
                }
                Err((invalid_index, message)) => {
                    selected = invalid_index;
                    error = Some(message);
                }
            },
            _ => {}
        }
    }
}

fn date_labels(editor: &DateEditor, now: DateTime<Local>, is_end: bool) -> Vec<String> {
    let defaults = [
        format!("{:04}", now.year()),
        format!("{:02}", now.month()),
        format!("{:02}", now.day()),
        format!("{:02}", now.hour()),
        format!("{:02}", now.minute()),
    ];
    let names = if zh() {
        ["年", "月", "日", "时", "分"]
    } else {
        ["year", "month", "day", "hour", "minute"]
    };
    let mut lines = Vec::with_capacity(6);
    for index in 0..DATE_FIELDS.len() {
        let entered = &editor.values[index];
        let display = if entered.is_empty() {
            "_".repeat(if index == 0 { 4 } else { 2 })
        } else {
            entered.clone()
        };
        lines.push(format!(
            "{:<4} {:<6} [{}]  ({})",
            DATE_FIELDS[index], names[index], display, defaults[index]
        ));
    }
    lines.push(
        if zh() {
            if is_end { "应用" } else { "下一页" }
        } else if is_end {
            "Apply"
        } else {
            "Next page"
        }
        .to_owned(),
    );
    lines
}

fn preset_range(index: usize, now: DateTime<Local>) -> Result<UsageRangeChoice> {
    let start = match index {
        0 => {
            // 1d means today since local midnight; 24h is a rolling window.
            let midnight = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("midnight is a valid civil time");
            Local.from_local_datetime(&midnight).earliest()
        }
        1 => Some(now - Duration::hours(24)),
        2 => Some(now - Duration::days(3)),
        3 => Some(now - Duration::days(7)),
        4 => now.checked_sub_months(Months::new(1)),
        5 => return Ok(UsageRangeChoice::All),
        _ => unreachable!("preset index should stay in 0..6"),
    }
    .ok_or_else(|| {
        CasError::Verification(invalid_date_message(
            "the selected time range is not representable in local time",
            "所选区间在本地时区中无法表示",
        ))
    })?;
    Ok(UsageRangeChoice::Bounded(UsageTimeRange::new(
        start.to_utc(),
        now.to_utc(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("representable local date")
    }

    #[test]
    fn presets_are_listed_vertically_with_manual_input_at_the_bottom() {
        let labels = preset_labels();
        assert_eq!(labels.len(), 7);
        for (index, preset) in PRESETS.iter().enumerate() {
            assert!(labels[index].starts_with(preset));
        }
        assert!(labels[6].contains(if zh() { "手动输入" } else { "Custom" }));
    }

    #[test]
    fn date_fields_are_vertical_and_have_a_last_action_row() {
        let now = local_time(2026, 10, 8, 15, 34);
        let lines = date_labels(&DateEditor::default(), now, false);
        assert_eq!(lines.len(), 6);
        for (line, field) in lines.iter().zip(DATE_FIELDS) {
            assert!(line.starts_with(field));
        }
        assert!(lines[0].contains("2026"));
        assert!(lines[4].contains("34"));
        assert!(!lines[5].contains("mm"));
    }

    #[test]
    fn today_and_rolling_24_hours_are_distinct() {
        let now = local_time(2026, 10, 8, 15, 34);
        let UsageRangeChoice::Bounded(today) = preset_range(0, now).unwrap() else {
            panic!("today must be bounded")
        };
        let UsageRangeChoice::Bounded(day) = preset_range(1, now).unwrap() else {
            panic!("24h must be bounded")
        };
        assert_eq!(today.start.with_timezone(&Local).hour(), 0);
        assert_eq!(day.start, (now - Duration::hours(24)).to_utc());
        assert_eq!(today.end, now.to_utc());
        assert_ne!(today.start, day.start);
        assert_eq!(preset_range(5, now).unwrap(), UsageRangeChoice::All);
    }

    #[test]
    fn three_days_seven_days_and_calendar_month() {
        let now = local_time(2026, 10, 8, 15, 34);
        for (choice, days) in [(2, 3), (3, 7)] {
            let UsageRangeChoice::Bounded(range) = preset_range(choice, now).unwrap() else {
                panic!("preset must be bounded")
            };
            assert_eq!(range.start, (now - Duration::days(days)).to_utc());
        }
        let UsageRangeChoice::Bounded(month) = preset_range(4, now).unwrap() else {
            panic!("1m must be bounded")
        };
        let last_month = month.start.with_timezone(&Local);
        assert_eq!(
            (last_month.year(), last_month.month(), last_month.day()),
            (2026, 9, 8)
        );
    }

    #[test]
    fn manual_fields_default_to_the_same_captured_current_instant() {
        let now = local_time(2026, 10, 8, 15, 34);
        let blank = DateEditor::default();
        assert_eq!(blank.resolve(now, false).unwrap(), now.to_utc());
        assert_eq!(blank.resolve(now, true).unwrap(), now.to_utc());
    }

    #[test]
    fn manual_date_editing_fills_partial_fields_and_checks_calendar() {
        let now = local_time(2026, 10, 8, 15, 34);
        let mut edit = DateEditor::default();
        for char_ in "2024".chars() {
            edit.edit_digit(0, char_);
        }
        for char_ in "02".chars() {
            edit.edit_digit(1, char_);
        }
        for char_ in "29".chars() {
            edit.edit_digit(2, char_);
        }
        assert_eq!(
            edit.resolve(now, false)
                .unwrap()
                .with_timezone(&Local)
                .date_naive(),
            NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()
        );
        edit.clear(0);
        assert!(edit.resolve(now, false).is_err()); // 2026 is not leap year
        edit.clear(2);
        assert!(edit.resolve(now, false).is_ok()); // defaults to current day 8
        edit.edit_digit(3, '2');
        edit.edit_digit(3, '5');
        assert_eq!(edit.resolve(now, false).unwrap_err().0, 3);
        edit.erase(3);
        assert!(edit.resolve(now, false).is_ok());
    }
}
