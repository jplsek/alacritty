//! Sparse command annotations attached to rows in the terminal buffer.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use crate::index::Line;

const MAX_COMMAND_MARKS: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandMark {
    pub command: Option<Arc<str>>,
    pub status: Option<i32>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CommandMarks {
    marks: VecDeque<(i64, CommandMark)>,
    // Whole-buffer scrolling moves every mark by the same amount.
    origin: i64,
    running: bool,
}

impl CommandMarks {
    pub fn iter(&self) -> impl Iterator<Item = (Line, &CommandMark)> + Clone {
        self.marks.iter().filter_map(|(position, mark)| {
            let line = i32::try_from(position - self.origin).ok()?;
            Some((Line(line), mark))
        })
    }

    pub fn start(&mut self, line: Line, command: Option<Arc<str>>) {
        if self.marks.len() >= MAX_COMMAND_MARKS {
            self.marks.pop_front();
        }
        self.running = true;
        self.marks
            .push_back((self.origin + i64::from(line.0), CommandMark { command, status: None }));
    }

    pub fn finish(&mut self, status: i32) {
        if std::mem::take(&mut self.running)
            && let Some((_, mark)) = self.marks.back_mut()
        {
            mark.status = Some(status);
        }
    }

    pub fn shift(&mut self, lines: i64) {
        if !self.marks.is_empty() {
            self.origin -= lines;
        }
    }

    pub fn scroll_region(&mut self, region: &Range<Line>, delta: i64, into_history: bool) {
        let first = self.origin + i64::from(region.start.0);
        let end = self.origin + i64::from(region.end.0);
        self.update(|position| {
            if position >= end || (!into_history && position < first) {
                Some(position)
            } else {
                let position = position + delta;
                ((into_history || position >= first) && position < end).then_some(position)
            }
        });
    }

    pub fn clear_region(&mut self, start: Line, end: Line) {
        let range = self.origin + i64::from(start.0)..self.origin + i64::from(end.0);
        self.update(|position| (!range.contains(&position)).then_some(position));
    }

    pub fn discard_history_before(&mut self, top: Line) {
        if self.marks.is_empty() {
            return;
        }
        let top = self.origin + i64::from(top.0);
        // The oldest marks are usually first. Leave out-of-order marks to be filtered on read.
        while self.marks.front().is_some_and(|(position, _)| *position < top) {
            self.marks.pop_front();
        }
        if self.marks.is_empty() {
            self.running = false;
            self.origin = 0;
        }
    }

    pub fn reflow_map(&self, len: usize) -> Option<Vec<usize>> {
        (!self.marks.is_empty()).then(|| vec![0; len])
    }

    pub fn reflow(&mut self, map: &[usize], old_lines: usize, new_len: usize) {
        let origin = self.origin;
        self.update(|position| {
            let old_index = i64::try_from(old_lines).ok()? - (position - origin) - 1;
            let new_index = *map.get(usize::try_from(old_index).ok()?)?;
            (new_index < new_len)
                .then_some(origin + old_lines as i64 - new_len as i64 + new_index as i64)
        });
    }

    fn update(&mut self, mut map: impl FnMut(i64) -> Option<i64>) {
        if self.running && self.marks.back().is_some_and(|(line, _)| map(*line).is_none()) {
            self.running = false;
        }
        self.marks.retain_mut(|(line, _)| match map(*line) {
            Some(new_line) => {
                *line = new_line;
                true
            },
            None => false,
        });
    }
}
