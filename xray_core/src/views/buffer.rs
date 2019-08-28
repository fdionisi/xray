use std::cell::Cell;
use std::cmp::{self, Ordering};
use std::ops::Range;
use std::rc::Rc;

use futures::{Poll, Stream};
use serde_json;

use crate::buffer::{Buffer, Point, SelectionSetId};
use crate::movement;
use crate::notify_cell::NotifyCell;
use crate::window::{View, WeakViewHandle, Window};
use crate::ReplicaId;

pub trait BufferViewDelegate {
    fn set_active_buffer_view(&mut self, buffer_view: WeakViewHandle<BufferView>);
}

pub struct BufferView {
    buffer: Rc<Buffer>,
    updates_tx: NotifyCell<()>,
    updates_rx: Box<dyn Stream<Item = (), Error = ()>>,
    dropped: NotifyCell<bool>,
    selection_set_id: SelectionSetId,
    height: Option<f64>,
    width: Option<f64>,
    line_height: f64,
    scroll_top: f64,
    vertical_margin: u32,
    horizontal_margin: u32,
    vertical_autoscroll: Option<AutoScrollRequest>,
    horizontal_autoscroll: Cell<Option<Range<Point>>>,
    delegate: Option<WeakViewHandle<dyn BufferViewDelegate>>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct SelectionProps {
    pub user_id: Option<ReplicaId>,
    pub start: Point,
    pub end: Point,
    pub reversed: bool,
    pub remote: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BufferViewAction {
    UpdateScrollTop {
        delta: f64,
    },
    SetDimensions {
        width: u64,
        height: u64,
    },
    Edit {
        text: String,
    },
    Backspace,
    Delete,
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    MoveToBeginningOfWord,
    MoveToEndOfWord,
    MoveToBeginningOfLine,
    MoveToEndOfLine,
    MoveToTop,
    MoveToBottom,
    SelectUp,
    SelectDown,
    SelectLeft,
    SelectRight,
    SelectTo {
        row: u32,
        column: u32,
    },
    SelectToBeginningOfWord,
    SelectToEndOfWord,
    SelectToBeginningOfLine,
    SelectToEndOfLine,
    SelectToTop,
    SelectToBottom,
    SelectWord,
    SelectLine,
    AddSelectionAbove,
    AddSelectionBelow,
    SetCursorPosition {
        row: u32,
        column: u32,
        autoscroll: bool,
    },
}

struct AutoScrollRequest {
    range: Range<Point>,
    center: bool,
}

impl BufferView {
    pub fn new(
        buffer: Rc<Buffer>,
        delegate: Option<WeakViewHandle<dyn BufferViewDelegate>>,
    ) -> Self {
        let selection_set_id = buffer
            .add_selection_set(vec![Point::zero()..Point::zero()])
            .unwrap();

        let updates_tx = NotifyCell::new(());
        let updates_rx = Box::new(updates_tx.observe().select(buffer.updates()));
        Self {
            updates_tx,
            updates_rx,
            buffer,
            selection_set_id,
            dropped: NotifyCell::new(false),
            height: None,
            width: None,
            line_height: 10.0,
            scroll_top: 0.0,
            vertical_margin: 2,
            horizontal_margin: 4,
            vertical_autoscroll: None,
            horizontal_autoscroll: Cell::new(None),
            delegate,
        }
    }

    pub fn set_height(&mut self, height: f64) -> &mut Self {
        debug_assert!(height >= 0_f64);
        self.height = Some(height);
        self.autoscroll_to_cursor(false);
        self.updated();
        self
    }

    pub fn set_width(&mut self, width: f64) -> &mut Self {
        debug_assert!(width >= 0_f64);
        self.width = Some(width);
        self.autoscroll_to_cursor(false);
        self.updated();
        self
    }

    pub fn set_line_height(&mut self, line_height: f64) -> &mut Self {
        debug_assert!(line_height > 0_f64);
        self.line_height = line_height;
        self.autoscroll_to_cursor(false);
        self.updated();
        self
    }

    pub fn set_scroll_top(&mut self, scroll_top: f64) -> &mut Self {
        debug_assert!(scroll_top >= 0_f64);
        self.scroll_top = scroll_top;
        self.vertical_autoscroll = None;
        self.horizontal_autoscroll.replace(None);
        self.updated();
        self
    }

    fn scroll_top(&self) -> f64 {
        let max_scroll_top = f64::from(self.buffer.max_point().unwrap().row) * self.line_height;
        self.scroll_top.min(max_scroll_top)
    }

    fn scroll_bottom(&self) -> f64 {
        self.scroll_top() + self.height.unwrap_or(0.0)
    }

    // pub fn save(&self) -> Box<Future<Item = (), Error = io::Error>> {
    //     Box::new(
    //         self.buffer
    //             .borrow()
    //             .save()
    //             .unwrap()
    //             .map_err(|err| {
    //                 io::Error::new(
    //                     io::ErrorKind::Other,
    //                     format!("BufferView().save(): error={:?}", err),
    //                 )
    //             })
    //             .into_future(),
    //     )
    // }

    pub fn edit(&mut self, text: &str) {
        let offset_ranges = self
            .selections()
            .iter()
            .map(|selection| {
                let reversed = selection.end < selection.start;

                let start = if reversed {
                    selection.end
                } else {
                    selection.start
                };

                let end = if reversed {
                    selection.start
                } else {
                    selection.end
                };

                start..end
            })
            .collect::<Vec<Range<Point>>>();

        self.buffer.edit_2d(offset_ranges, text).unwrap();

        self.mutate_selections(|_, selections| {
            selections
                .into_iter()
                .map(|range| {
                    let mut point = cmp::min(range.start, range.end);

                    let lines = text.split('\n').collect::<Vec<&str>>();
                    let num_lines = lines.len() as u32 - 1;
                    let last_line = *lines.last().unwrap();

                    point = Point::new(
                        point.row + num_lines,
                        point.column + (last_line.len() as u32),
                    );

                    point..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn backspace(&mut self) {
        if self.all_selections_are_empty() {
            self.select_left();
        }
        self.edit("");
    }

    pub fn delete(&mut self) {
        if self.all_selections_are_empty() {
            self.select_right();
        }
        self.edit("");
    }

    fn all_selections_are_empty(&self) -> bool {
        self.selections()
            .iter()
            .all(|selection| selection.start == selection.end)
    }

    fn mutate_selections<F>(&mut self, f: F)
    where
        F: FnOnce(&Buffer, &mut Vec<Range<Point>>) -> Vec<Range<Point>>,
    {
        self.buffer
            .mutate_selections(self.selection_set_id, f)
            .unwrap();
    }

    pub fn set_cursor_position(&mut self, position: Point, autoscroll: bool) {
        let position = self.buffer.clip_point(position).unwrap();

        self.mutate_selections(|_buffer, _selections| vec![position..position]);

        if autoscroll {
            self.autoscroll_to_cursor(false);
        }
    }

    pub fn add_selection(&mut self, start: Point, end: Point) {
        debug_assert!(start <= end); // TODO: Reverse selection if end < start
        let start = self.buffer.clip_point(start).unwrap();
        let end = self.buffer.clip_point(end).unwrap();

        self.buffer
            .add_selection_set(vec![Range { start, end }])
            .unwrap();

        self.autoscroll_to_cursor(false);
    }

    pub fn add_selection_above(&mut self) {
        self.mutate_selections(|buffer, selections| {
            let mut new_selections = Vec::new();

            for selection in selections.iter() {
                let selection_start = selection.start;
                let selection_end = selection.end;

                if selection_start.row != selection_end.row {
                    continue;
                }

                let goal_column = selection_end.column;
                let mut row = selection_start.row;
                while row > 0 {
                    row -= 1;
                    let max_column = buffer.len_for_row(row).unwrap();

                    let start_column;
                    let end_column;
                    let add_selection;
                    if selection_start == selection_end {
                        start_column = cmp::min(goal_column, max_column);
                        end_column = cmp::min(goal_column, max_column);
                        add_selection = selection_end.column == 0 || end_column > 0;
                    } else {
                        start_column = cmp::min(selection_start.column, max_column);
                        end_column = cmp::min(goal_column, max_column);
                        add_selection = start_column != end_column;
                    }

                    if add_selection {
                        new_selections.push(selection.start..selection.end);
                        new_selections
                            .push(Point::new(row, start_column)..Point::new(row, end_column));
                        break;
                    }
                }
            }
            new_selections
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn add_selection_below(&mut self) {
        self.mutate_selections(|buffer, selections| {
            let max_row = buffer.max_point().unwrap().row;

            let mut new_selections = Vec::new();
            for selection in selections.iter() {
                let selection_start = selection.start;
                let selection_end = selection.end;
                if selection_start.row != selection_end.row {
                    continue;
                }

                let goal_column = selection_end.column;
                let mut row = selection_start.row;
                while row < max_row {
                    row += 1;
                    let max_column = buffer.len_for_row(row).unwrap();

                    let start_column;
                    let end_column;
                    let add_selection;
                    if selection_start == selection_end {
                        start_column = cmp::min(goal_column, max_column);
                        end_column = cmp::min(goal_column, max_column);
                        add_selection = selection_end.column == 0 || end_column > 0;
                    } else {
                        start_column = cmp::min(selection_start.column, max_column);
                        end_column = cmp::min(goal_column, max_column);
                        add_selection = start_column != end_column;
                    }

                    if add_selection {
                        new_selections.push(selection.start..selection.end);
                        new_selections
                            .push(Point::new(row, start_column)..Point::new(row, end_column));
                        break;
                    }
                }
            }

            new_selections
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn move_left(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    if selection.start != selection.end {
                        selection.end..selection.end
                    } else {
                        let point = movement::left(&buffer, selection.end).unwrap();
                        point..point
                    }
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_left(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| selection.start..movement::left(&buffer, selection.end).unwrap())
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn move_right(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    if selection.start != selection.end {
                        selection.end..selection.end
                    } else {
                        let point = movement::right(&buffer, selection.end).unwrap();
                        point..point
                    }
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_right(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| selection.start..movement::right(&buffer, selection.end).unwrap())
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_to(&mut self, position: Point) {
        self.mutate_selections(|_buffer, selections| {
            selections
                .iter()
                .map(|selection| selection.start..position)
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn move_up(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let (point, _) =
                        movement::up(&buffer, selection.end, Some(selection.start.column)).unwrap();

                    point..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_up(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let (end, _) =
                        movement::up(&buffer, selection.end, Some(selection.start.column)).unwrap();
                    selection.start..end
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn move_down(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let (point, _) =
                        movement::down(&buffer, selection.end, Some(selection.start.column))
                            .unwrap();

                    point..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_down(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let (end, _) =
                        movement::down(&buffer, selection.end, Some(selection.start.column))
                            .unwrap();
                    selection.start..end
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn move_to_beginning_of_word(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let point = movement::beginning_of_word(buffer, selection.end).unwrap();
                    point..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn move_to_end_of_word(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let point = movement::end_of_word(buffer, selection.end).unwrap();
                    point..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_to_beginning_of_word(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let point = movement::beginning_of_word(buffer, selection.end).unwrap();
                    selection.start..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_to_end_of_word(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let point = movement::end_of_word(buffer, selection.end).unwrap();
                    selection.start..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_word(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let beginning = movement::beginning_of_word(buffer, selection.end).unwrap();
                    let end = movement::end_of_word(buffer, selection.end).unwrap();

                    beginning..end
                })
                .collect()
        });
    }

    pub fn move_to_beginning_of_line(&mut self) {
        self.mutate_selections(|_buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let new_head = movement::beginning_of_line(selection.end);
                    new_head..new_head
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn move_to_end_of_line(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    let new_head = movement::end_of_line(buffer, selection.end).unwrap();
                    new_head..new_head
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_to_beginning_of_line(&mut self) {
        self.mutate_selections(|_buffer, selections| {
            selections
                .iter()
                .map(|selection| selection.start..movement::beginning_of_line(selection.end))
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_to_end_of_line(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| {
                    selection.start..movement::end_of_line(buffer, selection.end).unwrap()
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_line(&mut self) {
        self.mutate_selections(|buffer, selections| {
            let max_point = buffer.max_point().unwrap();
            selections
                .iter()
                .map(|selection| {
                    let new_start = movement::beginning_of_line(selection.end);
                    let new_end = cmp::min(Point::new(new_start.row + 1, 0), max_point);
                    new_start..new_end
                })
                .collect()
        });
    }

    pub fn move_to_top(&mut self) {
        self.mutate_selections(|_, selections| {
            selections
                .iter()
                .map(|_| {
                    let point = Point::zero();
                    point..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn move_to_bottom(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|_| {
                    let point = buffer.max_point().unwrap();
                    point..point
                })
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_to_top(&mut self) {
        self.mutate_selections(|_buffer, selections| {
            selections
                .iter()
                .map(|selection| selection.start..Point::zero())
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn select_to_bottom(&mut self) {
        self.mutate_selections(|buffer, selections| {
            selections
                .iter()
                .map(|selection| selection.start..buffer.max_point().unwrap())
                .collect()
        });

        self.autoscroll_to_cursor(false);
    }

    pub fn selections(&self) -> Vec<Range<Point>> {
        self.buffer
            .selections(self.selection_set_id)
            .unwrap()
            .clone()
    }

    fn render_selections(&self, range: Range<Point>) -> Vec<SelectionProps> {
        let mut rendered_selections = Vec::new();

        for (user_id, selections) in self.buffer.selection_ranges().unwrap().remote {
            for remote_selection in selections {
                for selection in self.query_selections(&remote_selection, &range) {
                    let mut selection_prop = SelectionProps::from(selection);
                    selection_prop.set_user_id(user_id);
                    selection_prop.set_remote(true);

                    rendered_selections.push(selection_prop);
                }
            }
        }

        for (_, ranges) in self.buffer.selection_ranges().unwrap().local.iter() {
            for selection in self.query_selections(&ranges, &range) {
                rendered_selections.push(SelectionProps::from(selection));
            }
        }

        rendered_selections
    }

    fn query_selections<'a>(
        &self,
        selections: &'a [Range<Point>],
        range: &Range<Point>,
    ) -> &'a [Range<Point>] {
        let start = range.start;
        let start_index = match selections.binary_search_by(|probe| probe.start.cmp(&start)) {
            Ok(index) => index,
            Err(index) => {
                if index > 0 && selections[index - 1].end.cmp(&start) == Ordering::Greater {
                    index - 1
                } else {
                    index
                }
            }
        };

        if range.end > self.buffer.max_point().unwrap() {
            &selections[start_index..]
        } else {
            let end = range.end;
            let end_index = match selections.binary_search_by(|probe| probe.start.cmp(&end)) {
                Ok(index) => index,
                Err(index) => index,
            };

            &selections[start_index..end_index]
        }
    }

    fn autoscroll_to_cursor(&mut self, center: bool) {
        let anchor = {
            let selections = self.selections();
            let selection = selections.last().unwrap();
            selection.end
        };

        self.autoscroll_to_range(anchor..anchor, center)
    }

    fn flush_vertical_autoscroll_to_selection(&mut self) {
        if let Some(request) = self.vertical_autoscroll.take() {
            self.autoscroll_to_range(request.range, request.center)
        }
    }

    fn autoscroll_to_range(&mut self, range: Range<Point>, center: bool) {
        // Ensure points are valid even if we can't autoscroll immediately because
        // flush_vertical_autoscroll_to_selection unwraps.
        if let Some(height) = self.height {
            let desired_top;
            let desired_bottom;
            if center {
                let center_position =
                    ((range.start.row + range.end.row) as f64 / 2_f64) * self.line_height;
                desired_top = 0_f64.max(center_position - height / 2_f64);
                desired_bottom = center_position + height / 2_f64;
            } else {
                desired_top =
                    range.start.row.saturating_sub(self.vertical_margin) as f64 * self.line_height;
                desired_bottom =
                    range.end.row.saturating_add(self.vertical_margin) as f64 * self.line_height;
            }

            if self.scroll_top() > desired_top {
                self.set_scroll_top(desired_top);
            } else if self.scroll_bottom() < desired_bottom {
                self.set_scroll_top(desired_bottom - height);
            }

            self.horizontal_autoscroll.replace(Some(range));
        } else {
            self.vertical_autoscroll = Some(AutoScrollRequest { range, center });
            self.horizontal_autoscroll.replace(None);
        }
    }

    fn updated(&mut self) {
        self.updates_tx.set(());
    }
}

impl View for BufferView {
    fn component_name(&self) -> &'static str {
        "BufferView"
    }

    fn will_mount(&mut self, window: &mut Window, self_handle: WeakViewHandle<Self>) {
        self.height = Some(window.height());
        self.flush_vertical_autoscroll_to_selection();
        if let Some(ref delegate) = self.delegate {
            delegate.map(|delegate| delegate.set_active_buffer_view(self_handle));
        }
    }

    fn render(&self) -> serde_json::Value {
        let start = Point::new((self.scroll_top() / self.line_height).floor() as u32, 0);
        let end = Point::new((self.scroll_bottom() / self.line_height).ceil() as u32, 0);

        let mut lines = Vec::new();
        let mut cur_line = Vec::new();
        let mut cur_row = start.row;
        for c in self.buffer.iter_at_point(start).unwrap() {
            if c == u16::from(b'\n') {
                lines.push(String::from_utf16_lossy(&cur_line));
                cur_line = Vec::new();
                cur_row += 1;
                if cur_row >= end.row {
                    break;
                }
            } else {
                cur_line.push(c);
            }
        }
        if cur_row < end.row {
            lines.push(String::from_utf16_lossy(&cur_line));
        }

        let longest_row = self.buffer.longest_row().unwrap();
        let longest_line = if start.row <= longest_row && longest_row < end.row {
            lines[(longest_row - start.row) as usize].clone()
        } else {
            String::from_utf16_lossy(
                &self
                    .buffer
                    .line(self.buffer.longest_row().unwrap())
                    .unwrap(),
            )
        };

        let horizontal_autoscroll = self.horizontal_autoscroll.take().map(|range| {
            let scroll_start = range.start;
            let scroll_end = range.end;
            let start_line = if start.row <= scroll_start.row && scroll_start.row <= end.row {
                lines[(scroll_start.row - start.row) as usize].clone()
            } else {
                String::from_utf16_lossy(&self.buffer.line(scroll_start.row).unwrap())
            };
            let end_line = if start.row <= scroll_end.row && scroll_end.row <= end.row {
                lines[(scroll_end.row - start.row) as usize].clone()
            } else {
                String::from_utf16_lossy(&self.buffer.line(scroll_end.row).unwrap())
            };

            json!({
                "start": scroll_start,
                "start_line": start_line,
                "end": scroll_end,
                "end_line": end_line,
            })
        });

        json!({
            "first_visible_row": start.row,
            "total_row_count": self.buffer.max_point().unwrap().row + 1,
            "lines": lines,
            "longest_line": longest_line,
            "scroll_top": self.scroll_top(),
            "horizontal_autoscroll": horizontal_autoscroll,
            "horizontal_margin": self.horizontal_margin,
            "height": self.height,
            "width": self.width,
            "line_height": self.line_height,
            "selections": self.render_selections(start..end),
        })
    }

    fn dispatch_action(&mut self, action: serde_json::Value, _: &mut Window) {
        match serde_json::from_value(action) {
            Ok(BufferViewAction::UpdateScrollTop { delta }) => {
                let mut scroll_top = self.scroll_top() + delta;
                if scroll_top < 0.0 {
                    scroll_top = 0.0;
                }
                self.set_scroll_top(scroll_top);
            }
            Ok(BufferViewAction::SetDimensions { width, height }) => {
                self.set_width(width as f64);
                self.set_height(height as f64);
            }
            Ok(BufferViewAction::Edit { text }) => self.edit(text.as_str()),
            Ok(BufferViewAction::Backspace) => self.backspace(),
            Ok(BufferViewAction::Delete) => self.delete(),
            Ok(BufferViewAction::MoveUp) => self.move_up(),
            Ok(BufferViewAction::MoveDown) => self.move_down(),
            Ok(BufferViewAction::MoveLeft) => self.move_left(),
            Ok(BufferViewAction::MoveRight) => self.move_right(),
            Ok(BufferViewAction::MoveToBeginningOfWord) => self.move_to_beginning_of_word(),
            Ok(BufferViewAction::MoveToEndOfWord) => self.move_to_end_of_word(),
            Ok(BufferViewAction::MoveToBeginningOfLine) => self.move_to_beginning_of_line(),
            Ok(BufferViewAction::MoveToEndOfLine) => self.move_to_end_of_line(),
            Ok(BufferViewAction::MoveToTop) => self.move_to_top(),
            Ok(BufferViewAction::MoveToBottom) => self.move_to_bottom(),
            Ok(BufferViewAction::SelectUp) => self.select_up(),
            Ok(BufferViewAction::SelectDown) => self.select_down(),
            Ok(BufferViewAction::SelectLeft) => self.select_left(),
            Ok(BufferViewAction::SelectRight) => self.select_right(),
            Ok(BufferViewAction::SelectTo { row, column }) => {
                self.select_to(Point::new(row, column))
            }
            Ok(BufferViewAction::SelectToBeginningOfWord) => self.select_to_beginning_of_word(),
            Ok(BufferViewAction::SelectToEndOfWord) => self.select_to_end_of_word(),
            Ok(BufferViewAction::SelectToBeginningOfLine) => self.select_to_beginning_of_line(),
            Ok(BufferViewAction::SelectToEndOfLine) => self.select_to_end_of_line(),
            Ok(BufferViewAction::SelectToTop) => self.select_to_top(),
            Ok(BufferViewAction::SelectToBottom) => self.select_to_bottom(),
            Ok(BufferViewAction::SelectWord) => self.select_word(),
            Ok(BufferViewAction::SelectLine) => self.select_line(),
            Ok(BufferViewAction::AddSelectionAbove) => self.add_selection_above(),
            Ok(BufferViewAction::AddSelectionBelow) => self.add_selection_below(),
            Ok(BufferViewAction::SetCursorPosition {
                row,
                column,
                autoscroll,
            }) => self.set_cursor_position(Point::new(row, column), autoscroll),
            Err(action) => eprintln!("Unrecognized action {:?}", action),
        }
    }
}

impl Stream for BufferView {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.updates_rx.poll()
    }
}

impl Drop for BufferView {
    fn drop(&mut self) {
        self.buffer
            .remove_selection_set(self.selection_set_id)
            .unwrap();

        self.dropped.set(true);
    }
}

impl SelectionProps {
    fn set_user_id(&mut self, user_id: ReplicaId) {
        self.user_id = Some(user_id)
    }

    fn set_remote(&mut self, remote: bool) {
        self.remote = remote
    }
}

impl Default for SelectionProps {
    fn default() -> SelectionProps {
        SelectionProps {
            user_id: None,
            start: Point::zero(),
            end: Point::zero(),
            reversed: false,
            remote: false,
        }
    }
}

impl From<&Range<Point>> for SelectionProps {
    fn from(range: &Range<Point>) -> SelectionProps {
        let reversed = range.end < range.start;
        SelectionProps {
            start: if reversed { range.end } else { range.start },
            end: if reversed { range.start } else { range.end },
            reversed,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        buffer::{Buffer, Point},
        tests::buffer::TestBuffer,
        Error,
    };

    #[test]
    fn test_cursor_movement() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abc")?;
        editor.buffer.edit(vec![3..3], "\n")?;
        editor.buffer.edit(vec![4..4], "\ndef")?;
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        editor.move_right();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 1)]);

        // Wraps across lines moving right
        for _ in 0..3 {
            editor.move_right();
        }
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);

        // Stops at end
        for _ in 0..4 {
            editor.move_right();
        }
        assert_eq!(render_selections(&editor), vec![empty_selection(2, 3)]);

        // Wraps across lines moving left
        for _ in 0..4 {
            editor.move_left();
        }
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);

        // Stops at start
        for _ in 0..4 {
            editor.move_left();
        }
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        // Moves down and up at column 0
        editor.move_down();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        editor.move_up();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        // // Maintains a goal column when moving down
        // // This means we'll jump to the column we started with even after crossing a shorter line
        // editor.move_right();
        // editor.move_right();
        // editor.move_down();
        // assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        // editor.move_down();
        // assert_eq!(render_selections(&editor), vec![empty_selection(2, 2)]);

        // Jumps to end when moving down on the last line.
        // editor.move_down();
        // assert_eq!(render_selections(&editor), vec![empty_selection(2, 3)]);
        //
        // // Stops at end
        // editor.move_down();
        // assert_eq!(render_selections(&editor), vec![empty_selection(2, 3)]);
        //
        // // Resets the goal column when moving horizontally
        // editor.move_left();
        // editor.move_left();
        // editor.move_up();
        // assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        // editor.move_up();
        // assert_eq!(render_selections(&editor), vec![empty_selection(0, 1)]);
        //
        // // Jumps to start when moving up on the first line
        // editor.move_up();
        // assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        // // Preserves goal column after jumping to start/end
        // editor.move_down();
        // editor.move_down();
        // assert_eq!(render_selections(&editor), vec![empty_selection(2, 1)]);
        // editor.move_down();
        // assert_eq!(render_selections(&editor), vec![empty_selection(2, 3)]);
        // editor.move_up();
        // editor.move_up();
        // assert_eq!(render_selections(&editor), vec![empty_selection(0, 1)]);

        Ok(())
    }

    // #[test]
    // fn open_same_buffer() -> Result<(), Error> {
    //     let buffer = Buffer::basic();
    //
    //     let mut editor_1 = BufferView::new(
    //         buffer.clone(),
    //         None,
    //     );
    //
    //     let mut editor_2 = BufferView::new(
    //         buffer.clone(),
    //         None,
    //     );
    //
    //     editor_1.edit("a");
    //     editor_2.move_right();
    //     editor_2.edit("b");
    //
    //     assert_eq!(editor_1.buffer.to_string(), String::from("ab"));
    //     assert_eq!(editor_1.render_selections(Point::zero()..Point::new(1, 0)), vec![]);
    //
    //     Ok(())
    // }

    #[test]
    fn test_selection_movement() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abc")?;
        editor.buffer.edit(vec![3..3], "\n")?;
        editor.buffer.edit(vec![4..4], "\ndef")?;

        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        editor.select_right();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (0, 1))]);

        // Selecting right wraps across newlines
        for _ in 0..3 {
            editor.select_right();
        }
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (1, 0))]);

        // Moving right with a non-empty selection clears the selection
        editor.move_right();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        editor.move_right();
        assert_eq!(render_selections(&editor), vec![empty_selection(2, 0)]);

        // Selecting left wraps across newlines
        editor.select_left();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((1, 0), (2, 0))]
        );
        editor.select_left();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 3), (2, 0))]
        );

        // Moving left with a non-empty selection clears the selection
        editor.move_left();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 3)]);

        // Reverse is updated correctly when selecting left and right
        editor.select_left();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 2), (0, 3))]
        );
        editor.select_right();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 3)]);
        editor.select_right();
        assert_eq!(render_selections(&editor), vec![selection((0, 3), (1, 0))]);
        editor.select_left();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 3)]);
        editor.select_left();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 2), (0, 3))]
        );

        // // Selecting vertically moves the head and updates the reversed property
        // editor.select_left();
        // assert_eq!(
        //     render_selections(&editor),
        //     vec![rev_selection((0, 1), (0, 3))]
        // );
        // editor.select_down();
        // assert_eq!(render_selections(&editor), vec![selection((0, 3), (1, 0))]);
        // editor.select_down();
        // assert_eq!(render_selections(&editor), vec![selection((0, 3), (2, 1))]);
        // editor.select_up();
        // editor.select_up();
        // assert_eq!(
        //     render_selections(&editor),
        //     vec![rev_selection((0, 1), (0, 3))]
        // );
        //
        // // Favors selection end when moving down
        // editor.move_down();
        // editor.move_down();
        // assert_eq!(render_selections(&editor), vec![empty_selection(2, 3)]);
        //
        // // Favors selection start when moving up
        // editor.move_left();
        // editor.move_left();
        // editor.select_right();
        // editor.select_right();
        // assert_eq!(render_selections(&editor), vec![selection((2, 1), (2, 3))]);
        // editor.move_up();
        // editor.move_up();
        // assert_eq!(render_selections(&editor), vec![empty_selection(0, 1)]);
        //
        // // Select to a direct point in front of cursor position
        // editor.select_to(Point::new(1, 0));
        // assert_eq!(render_selections(&editor), vec![selection((0, 1), (1, 0))]);
        // editor.move_right(); // cancel selection
        // assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        // editor.move_right();
        // editor.move_right();
        // assert_eq!(render_selections(&editor), vec![empty_selection(2, 1)]);
        //
        // // Selection can even go to a point before the cursor (with reverse)
        // editor.select_to(Point::new(0, 0));
        // assert_eq!(
        //     render_selections(&editor),
        //     vec![rev_selection((0, 0), (2, 1))]
        // );
        //
        // // A selection can switch to a new point and the selection will update
        // editor.select_to(Point::new(0, 3));
        // assert_eq!(
        //     render_selections(&editor),
        //     vec![rev_selection((0, 3), (2, 1))]
        // );
        //
        // // A selection can even swing around the cursor without having to unselect
        // editor.select_to(Point::new(2, 3));
        // assert_eq!(render_selections(&editor), vec![selection((2, 1), (2, 3))]);

        Ok(())
    }

    #[test]
    fn test_move_to_beginning_or_end_of_word() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abc def\nghi.jkl")?;
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        editor.move_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 3)]);
        editor.move_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 4)]);
        editor.move_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 7)]);
        editor.move_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        editor.move_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 3)]);
        editor.move_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 4)]);
        editor.move_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 7)]);
        editor.move_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 7)]);

        editor.move_to_beginning_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 4)]);
        editor.move_to_beginning_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 3)]);
        editor.move_to_beginning_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        editor.move_to_beginning_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 7)]);
        editor.move_to_beginning_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 4)]);
        editor.move_to_beginning_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 3)]);
        editor.move_to_beginning_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);
        editor.move_to_beginning_of_word();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        Ok(())
    }

    #[test]
    fn test_select_to_beginning_or_end_of_word() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abc def\nghi.jkl")?;
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        editor.select_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (0, 3))]);
        editor.select_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (0, 4))]);
        editor.select_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (0, 7))]);
        editor.select_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (1, 0))]);
        editor.select_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (1, 3))]);
        editor.select_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (1, 4))]);
        editor.select_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (1, 7))]);
        editor.select_to_end_of_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (1, 7))]);

        editor.move_right();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 7)]);

        editor.select_to_beginning_of_word();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((1, 4), (1, 7))]
        );
        editor.select_to_beginning_of_word();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((1, 3), (1, 7))]
        );
        editor.select_to_beginning_of_word();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((1, 0), (1, 7))]
        );
        editor.select_to_beginning_of_word();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 7), (1, 7))]
        );
        editor.select_to_beginning_of_word();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 4), (1, 7))]
        );
        editor.select_to_beginning_of_word();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 3), (1, 7))]
        );
        editor.select_to_beginning_of_word();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 0), (1, 7))]
        );
        editor.select_to_beginning_of_word();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 0), (1, 7))]
        );

        Ok(())
    }

    #[test]
    fn test_move_to_beginning_or_end_of_line() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abcdef\nghijklmno")?;
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        editor.move_to_end_of_line();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 6)]);
        editor.move_to_end_of_line();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 6)]);
        editor.move_right();
        editor.move_to_end_of_line();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 9)]);

        editor.move_to_beginning_of_line();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        editor.move_to_beginning_of_line();
        assert_eq!(render_selections(&editor), vec![empty_selection(1, 0)]);
        editor.move_left();
        editor.move_to_beginning_of_line();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        Ok(())
    }

    #[test]
    fn test_select_to_beginning_or_end_of_line() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abcdef\nghijklmno")?;
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        editor.select_to_end_of_line();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (0, 6))]);
        editor.select_to_end_of_line();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (0, 6))]);
        editor.move_right();
        editor.move_right();
        editor.select_to_end_of_line();
        assert_eq!(render_selections(&editor), vec![selection((1, 0), (1, 9))]);

        editor.move_right();
        editor.select_to_beginning_of_line();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((1, 0), (1, 9))]
        );
        editor.select_to_beginning_of_line();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((1, 0), (1, 9))]
        );
        editor.move_left();
        editor.move_left();
        editor.select_to_beginning_of_line();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 0), (0, 6))]
        );

        Ok(())
    }

    #[test]
    fn test_select_word() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abc.def---ghi")?;

        editor.set_cursor_position(Point::new(0, 5), false);
        editor.select_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 4), (0, 7))]);

        editor.set_cursor_position(Point::new(0, 8), false);
        editor.select_word();
        assert_eq!(render_selections(&editor), vec![selection((0, 7), (0, 10))]);

        Ok(())
    }

    #[test]
    fn test_select_line() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abc\ndef\nghi")?;

        editor.set_cursor_position(Point::new(0, 2), false);
        editor.select_line();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (1, 0))]);

        editor.set_cursor_position(Point::new(2, 1), false);
        editor.select_line();
        assert_eq!(render_selections(&editor), vec![selection((2, 0), (2, 3))]);

        Ok(())
    }

    #[test]
    fn test_move_to_top_or_bottom() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abc\ndef\nghi")?;
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        editor.move_to_bottom();
        assert_eq!(render_selections(&editor), vec![empty_selection(2, 3)]);
        editor.move_to_top();
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        Ok(())
    }

    #[test]
    fn test_select_to_top_or_bottom() -> Result<(), Error> {
        let mut editor = BufferView::new(Buffer::basic(), None);
        editor.buffer.edit(vec![0..0], "abc\ndef\nghi")?;
        assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);

        editor.select_to_bottom();
        assert_eq!(render_selections(&editor), vec![selection((0, 0), (2, 3))]);

        editor.move_right();
        editor.select_to_top();
        assert_eq!(
            render_selections(&editor),
            vec![rev_selection((0, 0), (2, 3))]
        );

        Ok(())
    }

    // FIXME
    //
    // #[test]
    // fn test_backspace() -> Result<(), Error> {
    //     let mut editor = BufferView::new(
    //         Buffer::basic(),
    //         None,
    //     );
    //     editor.buffer.edit(vec![0..0], "abcdefghi")?;
    //     editor.add_selection(Point::new(0, 3), Point::new(0, 4));
    //     editor.add_selection(Point::new(0, 9), Point::new(0, 9));
    //     editor.backspace();
    //     assert_eq!(editor.buffer.to_string(), "abcefghi");
    //     editor.backspace();
    //     assert_eq!(editor.buffer.to_string(), "abefgh");
    //
    //     Ok(())
    // }

    // FIXME
    //
    // #[test]
    // fn test_delete() -> Result<(), Error> {
    //     let mut editor = BufferView::new(
    //         Buffer::basic(),
    //         None,
    //     );
    //     editor.buffer.edit(vec![0..0], "abcdefghi")?;
    //     editor.add_selection(Point::new(0, 3), Point::new(0, 4));
    //     editor.add_selection(Point::new(0, 9), Point::new(0, 9));
    //     editor.delete();
    //     assert_eq!(editor.buffer.to_string(), "abcefghi");
    //     editor.delete();
    //     assert_eq!(editor.buffer.to_string(), "bcfghi");
    //
    //     Ok(())
    // }

    // FIXME
    //
    // #[test]
    // fn test_add_selection() -> Result<(), Error> {
    //     let mut editor = BufferView::new(
    //         Buffer::basic(),
    //         None,
    //     );
    //     editor
    //         .buffer
    //         .edit(vec![0..0], "abcd\nefgh\nijkl\nmnop")?;
    //     assert_eq!(render_selections(&editor), vec![empty_selection(0, 0)]);
    //
    //     // Adding non-overlapping selections
    //     editor.move_right();
    //     editor.move_right();
    //     editor.add_selection(Point::new(0, 0), Point::new(0, 1));
    //     editor.add_selection(Point::new(2, 2), Point::new(2, 3));
    //     editor.add_selection(Point::new(0, 3), Point::new(1, 2));
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             selection((0, 0), (0, 1)),
    //             selection((0, 2), (0, 2)),
    //             selection((0, 3), (1, 2)),
    //             selection((2, 2), (2, 3)),
    //         ]
    //     );
    //
    //     // Adding a selection that starts at the start of an existing selection
    //     editor.add_selection(Point::new(0, 3), Point::new(1, 0));
    //     editor.add_selection(Point::new(0, 3), Point::new(1, 3));
    //     editor.add_selection(Point::new(0, 3), Point::new(1, 2));
    //
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             selection((0, 0), (0, 1)),
    //             selection((0, 2), (0, 2)),
    //             selection((0, 3), (1, 3)),
    //             selection((2, 2), (2, 3)),
    //         ]
    //     );
    //
    //     // Adding a selection that starts or ends inside an existing selection
    //     editor.add_selection(Point::new(0, 1), Point::new(0, 2));
    //     editor.add_selection(Point::new(1, 2), Point::new(1, 4));
    //     editor.add_selection(Point::new(2, 1), Point::new(2, 2));
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             selection((0, 0), (0, 2)),
    //             selection((0, 3), (1, 4)),
    //             selection((2, 1), (2, 3)),
    //         ]
    //     );
    //
    //     Ok(())
    // }

    // #[test]
    // fn test_add_selection_above() {
    //     let mut editor = BufferView::new(
    //         Rc::new(RefCell::new(Buffer::new(build_base_buffer_id()))),
    //         0,
    //         None,
    //     );
    //     editor.buffer.borrow_mut().edit(
    //         &[0..0],
    //         "\
    //          abcdefghijk\n\
    //          lmnop\n\
    //          \n\
    //          \n\
    //          qrstuvwxyz\n\
    //          ",
    //     );
    //
    //     // Multi-line selections
    //     editor.move_down();
    //     editor.move_right();
    //     editor.move_right();
    //     editor.select_down();
    //     editor.select_down();
    //     editor.select_down();
    //     editor.select_right();
    //     editor.select_right();
    //     editor.add_selection_above();
    //     assert_eq!(render_selections(&editor), vec![selection((1, 2), (4, 4))]);
    //
    //     // Single-line selections
    //     editor.move_up();
    //     editor.move_left();
    //     editor.move_left();
    //     editor.add_selection(Point::new(2, 0), Point::new(2, 0));
    //     editor.add_selection(Point::new(4, 1), Point::new(4, 3));
    //     editor.add_selection(Point::new(4, 6), Point::new(4, 6));
    //     editor.add_selection(Point::new(4, 7), Point::new(4, 9));
    //     editor.add_selection_above();
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             selection((0, 0), (0, 0)),
    //             selection((0, 7), (0, 9)),
    //             selection((1, 0), (1, 0)),
    //             selection((1, 1), (1, 3)),
    //             selection((1, 5), (1, 5)),
    //             selection((2, 0), (2, 0)),
    //             selection((4, 1), (4, 3)),
    //             selection((4, 6), (4, 6)),
    //             selection((4, 7), (4, 9)),
    //         ]
    //     );
    //
    //     editor.add_selection_above();
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             selection((0, 0), (0, 0)),
    //             selection((0, 1), (0, 3)),
    //             selection((0, 6), (0, 6)),
    //             selection((0, 7), (0, 9)),
    //             selection((1, 0), (1, 0)),
    //             selection((1, 1), (1, 3)),
    //             selection((1, 5), (1, 5)),
    //             selection((2, 0), (2, 0)),
    //             selection((4, 1), (4, 3)),
    //             selection((4, 6), (4, 6)),
    //             selection((4, 7), (4, 9)),
    //         ]
    //     );
    // }
    //
    // #[test]
    // fn test_add_selection_below() {
    //     let mut editor = BufferView::new(
    //         Rc::new(RefCell::new(Buffer::new(build_base_buffer_id()))),
    //         0,
    //         None,
    //     );
    //     editor.buffer.borrow_mut().edit(
    //         &[0..0],
    //         "\
    //          abcdefgh\n\
    //          ijklm\n\
    //          \n\
    //          \n\
    //          nopqrstuvwx\n\
    //          yz\
    //          ",
    //     );
    //
    //     // Multi-line selections
    //     editor.select_down();
    //     editor.select_down();
    //     editor.select_down();
    //     editor.select_down();
    //     editor.select_right();
    //     editor.add_selection_below();
    //     assert_eq!(render_selections(&editor), vec![selection((0, 0), (4, 1))]);
    //
    //     // Single-line selections
    //     editor.move_left();
    //     editor.add_selection(Point::new(0, 1), Point::new(0, 1));
    //     editor.add_selection(Point::new(0, 4), Point::new(0, 8));
    //     editor.add_selection(Point::new(4, 5), Point::new(4, 6));
    //     editor.add_selection_below();
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             selection((0, 0), (0, 0)),
    //             selection((0, 1), (0, 1)),
    //             selection((0, 4), (0, 8)),
    //             selection((1, 0), (1, 0)),
    //             selection((1, 1), (1, 1)),
    //             selection((1, 4), (1, 5)),
    //             selection((4, 5), (4, 6)),
    //         ]
    //     );
    //
    //     editor.add_selection_below();
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             selection((0, 0), (0, 0)),
    //             selection((0, 1), (0, 1)),
    //             selection((0, 4), (0, 8)),
    //             selection((1, 0), (1, 0)),
    //             selection((1, 1), (1, 1)),
    //             selection((1, 4), (1, 5)),
    //             selection((2, 0), (2, 0)),
    //             selection((4, 1), (4, 1)),
    //             selection((4, 4), (4, 8)),
    //         ]
    //     );
    // }
    //
    // #[test]
    // fn test_set_cursor_position() {
    //     let mut editor = BufferView::new(
    //         Rc::new(RefCell::new(Buffer::new(build_base_buffer_id()))),
    //         0,
    //         None,
    //     );
    //     editor.buffer.borrow_mut().edit(&[0..0], "abc\ndef\nghi");
    //     editor.add_selection_below();
    //     editor.add_selection_below();
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             empty_selection(0, 0),
    //             empty_selection(1, 0),
    //             empty_selection(2, 0),
    //         ]
    //     );
    //
    //     editor.set_cursor_position(Point::new(1, 2), false);
    //     assert_eq!(render_selections(&editor), vec![empty_selection(1, 2)]);
    // }

    // FIXME
    //
    // #[test]
    // fn test_edit() -> Result<(), Error> {
    //     let mut editor = BufferView::new(
    //         Buffer::basic(),
    //         None,
    //     );
    //
    //     editor
    //         .buffer
    //         .edit(vec![0..0], "abcdefgh\nhijklmno")?;
    //
    //     // Three selections on the same line
    //     editor.select_right();
    //     editor.select_right();
    //     editor.add_selection(Point::new(0, 3), Point::new(0, 5));
    //     editor.add_selection(Point::new(0, 7), Point::new(1, 1));
    //     editor.edit("-");
    //     assert_eq!(editor.buffer.to_string(), "-c-fg-ijklmno");
    //     assert_eq!(
    //         render_selections(&editor),
    //         vec![
    //             selection((0, 1), (0, 1)),
    //             selection((0, 3), (0, 3)),
    //             selection((0, 6), (0, 6)),
    //         ]
    //     );
    //
    //     let mut editor = BufferView::new(
    //         Buffer::basic(),
    //         None,
    //     );
    //     editor.buffer.edit(vec![0..0], "123")?;
    //
    //     editor.edit("ä");
    //     editor.edit("a");
    //
    //     assert_eq!(editor.buffer.to_string(), "äa123");
    //     assert_eq!(render_selections(&editor), vec![selection((0, 2), (0, 2)),]);
    //
    //     Ok(())
    // }

    #[test]
    fn test_autoscroll() -> Result<(), Error> {
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "abc\ndef\nghi\njkl\nmno\npqr\nstu\nvwx\nyz")?;
        let start = Point::zero();
        let end = Point::new(8, 2);
        let max_point = buffer.max_point()?;
        let mut editor = BufferView::new(buffer, None);
        let line_height = 5.0;
        let height = 3.0 * line_height;
        editor
            .set_height(height)
            .set_line_height(line_height)
            .set_scroll_top(2.5 * line_height);
        assert_eq!(editor.scroll_top(), 2.5 * line_height);

        editor.autoscroll_to_range(start.clone()..start.clone(), true);
        assert_eq!(editor.scroll_top(), 0.0);
        editor.autoscroll_to_range(end.clone()..end.clone(), true);
        assert_eq!(
            editor.scroll_top(),
            (max_point.row as f64 * line_height) - (height / 2.0)
        );

        Ok(())
    }

    // FIXME
    //
    // #[test]
    // fn test_render() -> Result<(), Error> {
    //     let buffer = Buffer::basic();
    //     buffer
    //         .edit(vec![0..0], "abc\ndef\nghi\njkl\nmno\npqr\nstu\nvwx\nyz")?;
    //     let line_height = 6.0;
    //
    //     {
    //         let mut editor = BufferView::new(buffer.clone(), None);
    //         // Selections starting or ending outside viewport
    //         editor.add_selection(Point::new(1, 2), Point::new(3, 1));
    //         editor.add_selection(Point::new(5, 2), Point::new(6, 0));
    //         // Selection fully inside viewport
    //         editor.add_selection(Point::new(3, 2), Point::new(4, 1));
    //         // Selection fully outside viewport
    //         editor.add_selection(Point::new(6, 3), Point::new(7, 2));
    //         editor
    //             .set_height(3.0 * line_height)
    //             .set_line_height(line_height)
    //             .set_scroll_top(2.5 * line_height);
    //
    //         let frame = editor.render();
    //         assert_eq!(frame["first_visible_row"], 2);
    //         assert_eq!(
    //             stringify_lines(&frame["lines"]),
    //             vec!["ghi", "jkl", "mno", "pqr"]
    //         );
    //         assert_eq!(
    //             frame["selections"],
    //             json!([
    //                 selection((1, 2), (3, 1)),
    //                 selection((3, 2), (4, 1)),
    //                 selection((5, 2), (6, 0)),
    //             ])
    //         );
    //     }
    //
    //     // Selection starting at the end of buffer
    //     {
    //         let mut editor = BufferView::new(buffer.clone(), None);
    //         editor.add_selection(Point::new(8, 2), Point::new(8, 2));
    //         editor
    //             .set_height(8.0 * line_height)
    //             .set_line_height(line_height)
    //             .set_scroll_top(1.0 * line_height);
    //
    //         let frame = editor.render();
    //         assert_eq!(frame["first_visible_row"], 1);
    //         assert_eq!(
    //             stringify_lines(&frame["lines"]),
    //             vec!["def", "ghi", "jkl", "mno", "pqr", "stu", "vwx", "yz"]
    //         );
    //         assert_eq!(frame["selections"], json!([selection((8, 2), (8, 2))]));
    //     }
    //
    //     // Selection ending exactly at first visible row
    //     {
    //         let mut editor = BufferView::new(buffer.clone(), None);
    //         editor.add_selection(Point::new(0, 2), Point::new(1, 0));
    //         editor
    //             .set_height(3.0 * line_height)
    //             .set_line_height(line_height)
    //             .set_scroll_top(1.0 * line_height);
    //
    //         let frame = editor.render();
    //         assert_eq!(frame["first_visible_row"], 1);
    //         assert_eq!(stringify_lines(&frame["lines"]), vec!["def", "ghi", "jkl"]);
    //         assert_eq!(frame["selections"], json!([]));
    //     }
    //
    //     Ok(())
    // }

    #[test]
    fn test_longest_line_in_frame() -> Result<(), Error> {
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "1\n1\n1\n1\n11\n1\n1")?;
        let line_height = 6.0;

        let mut editor = BufferView::new(buffer.clone(), None);
        editor
            .set_height(2.0 * line_height)
            .set_line_height(line_height)
            .set_scroll_top(2.0 * line_height);

        let _ = editor.render();

        Ok(())
    }

    // FIXME
    //
    // #[test]
    // fn test_render_past_last_line() -> Result<(), Error> {
    //     let mut editor = BufferView::new(
    //         Buffer::basic(),
    //         None,
    //     );
    //
    //     let line_height = 4.0;
    //     editor.buffer.edit(vec![0..0], "abc\ndef\nghi")?;
    //     editor.add_selection(Point::new(2, 3), Point::new(2, 3));
    //     editor
    //         .set_height(3.0 * line_height)
    //         .set_line_height(line_height)
    //         .set_scroll_top(2.0 * line_height);
    //
    //     let frame = editor.render();
    //     assert_eq!(frame["first_visible_row"], 2);
    //     assert_eq!(stringify_lines(&frame["lines"]), vec!["ghi"]);
    //     assert_eq!(frame["selections"], json!([selection((2, 3), (2, 3))]));
    //
    //     editor.set_scroll_top(3.0 * line_height);
    //     let frame = editor.render();
    //     assert_eq!(frame["first_visible_row"], 2);
    //     assert_eq!(stringify_lines(&frame["lines"]), vec!["ghi"]);
    //     assert_eq!(frame["selections"], json!([selection((2, 3), (2, 3))]));
    //
    //     Ok(())
    // }

    // FIXME
    //
    #[test]
    fn test_dropping_view_removes_selection_set() -> Result<(), Error> {
        let buffer = Buffer::basic();
        let editor = BufferView::new(buffer.clone(), None);
        let selection_set_id = editor.selection_set_id;
        assert!(buffer.selections(selection_set_id).is_ok());

        drop(editor);
        assert!(buffer.selections(selection_set_id).is_err());

        Ok(())
    }

    fn stringify_lines(lines: &serde_json::Value) -> Vec<String> {
        lines
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().into())
            .collect()
    }

    fn render_selections(editor: &BufferView) -> Vec<SelectionProps> {
        editor
            .selections()
            .iter()
            .map(|s| SelectionProps::from(s))
            .collect()
    }

    fn empty_selection(row: u32, column: u32) -> SelectionProps {
        let range = Point::new(row, column)..Point::new(row, column);
        SelectionProps::from(&range)
    }

    fn selection(start: (u32, u32), end: (u32, u32)) -> SelectionProps {
        SelectionProps {
            user_id: None,
            start: Point::new(start.0, start.1),
            end: Point::new(end.0, end.1),
            reversed: false,
            remote: false,
        }
    }

    fn rev_selection(start: (u32, u32), end: (u32, u32)) -> SelectionProps {
        SelectionProps {
            user_id: None,
            start: Point::new(start.0, start.1),
            end: Point::new(end.0, end.1),
            reversed: true,
            remote: false,
        }
    }
}
