use super::*;
use crate::terminal::cells::*;
use melib::error::{MeliError, Result};
use nix::sys::wait::WaitStatus;
use nix::sys::wait::{waitpid, WaitPidFlag};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct EmbedGrid {
    cursor: (usize, usize),
    pub grid: CellBuffer,
    pub state: State,
    pub stdin: std::fs::File,
    pub child_pid: nix::unistd::Pid,
    pub terminal_size: (usize, usize),
    resized: bool,
}

impl EmbedGrid {
    pub fn new(stdin: std::fs::File, child_pid: nix::unistd::Pid) -> Self {
        EmbedGrid {
            cursor: (0, 0),
            terminal_size: (0, 0),
            grid: CellBuffer::default(),
            state: State::Normal,
            stdin,
            child_pid,
            resized: false,
        }
    }

    pub fn set_terminal_size(&mut self, new_val: (usize, usize)) {
        self.terminal_size = new_val;
        self.grid.resize(new_val.0, new_val.1, Cell::default());
        self.cursor = (0, 0);
        nix::sys::signal::kill(self.child_pid, nix::sys::signal::SIGWINCH).unwrap();
        self.resized = true;
    }

    pub fn wake_up(&self) {
        nix::sys::signal::kill(self.child_pid, nix::sys::signal::SIGCONT).unwrap();
    }

    pub fn stop(&self) {
        debug!("stopping");
        nix::sys::signal::kill(debug!(self.child_pid), nix::sys::signal::SIGSTOP).unwrap();
    }

    pub fn is_active(&self) -> Result<WaitStatus> {
        debug!(waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG),))
            .map_err(|e| MeliError::new(e.to_string()))
    }

    pub fn process_byte(&mut self, byte: u8) {
        let EmbedGrid {
            ref mut cursor,
            ref terminal_size,
            ref mut grid,
            ref mut state,
            ref mut stdin,
            ref mut resized,
            child_pid: _,
        } = self;

        macro_rules! increase_cursor_x {
            () => {
                if *cursor == *terminal_size {
                    /* do nothing */
                } else if cursor.0 + 1 >= terminal_size.0 {
                    //cursor.0 = 0;
                    //cursor.1 += 1;
                } else {
                    cursor.0 += 1;
                }
            };
        }

        let mut state = state;
        match (byte, &mut state) {
            (b'\x1b', State::Normal) => {
                *state = State::ExpectingControlChar;
            }
            (b']', State::ExpectingControlChar) => {
                let buf1 = Vec::new();
                *state = State::Osc1(buf1);
            }
            (b'[', State::ExpectingControlChar) => {
                *state = State::Csi;
            }
            (b'(', State::ExpectingControlChar) => {
                *state = State::G0;
            }
            (b'J', State::ExpectingControlChar) => {
                // "ESCJ Erase from the cursor to the end of the screen"
                debug!("sending {}", EscCode::from((&(*state), byte)));
                debug!("erasing from {:?} to {:?}", cursor, terminal_size);
                for y in cursor.1..terminal_size.1 {
                    for x in cursor.0..terminal_size.0 {
                        grid[(x, y)] = Cell::default();
                    }
                }
                *state = State::Normal;
            }
            (b'K', State::ExpectingControlChar) => {
                // "ESCK Erase from the cursor to the end of the line"
                debug!("sending {}", EscCode::from((&(*state), byte)));
                for x in cursor.0..terminal_size.0 {
                    grid[(x, cursor.1)] = Cell::default();
                }
                *state = State::Normal;
            }
            (c, State::ExpectingControlChar) => {
                debug!(
                    "unrecognised: byte is {} and state is {:?}",
                    byte as char, state
                );
                *state = State::Normal;
            }
            (b'?', State::Csi) => {
                let buf1 = Vec::new();
                *state = State::CsiQ(buf1);
            }
            /* ********** */
            /* ********** */
            /* ********** */
            /* OSC stuff */
            (c, State::Osc1(ref mut buf)) if (c >= b'0' && c <= b'9') || c == b'?' => {
                buf.push(c);
            }
            (b';', State::Osc1(ref mut buf1_p)) => {
                let buf1 = std::mem::replace(buf1_p, Vec::new());
                let buf2 = Vec::new();
                *state = State::Osc2(buf1, buf2);
            }
            (c, State::Osc2(_, ref mut buf)) if (c >= b'0' && c <= b'9') || c == b'?' => {
                buf.push(c);
            }
            (c, State::Osc1(_)) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (c, State::Osc2(_, _)) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            /* END OF OSC */
            /* ********** */
            /* ********** */
            /* ********** */
            /* ********** */
            (b'\r', State::Normal) => {
                //debug!("setting cell {:?} char '{}'", cursor, c as char);
                debug!("carriage return x-> 0, cursor was: {:?}", cursor);
                cursor.0 = 0;
                debug!("cursor became: {:?}", cursor);
            }
            (b'\n', State::Normal) => {
                //debug!("setting cell {:?} char '{}'", cursor, c as char);
                debug!("newline y-> y+1, cursor was: {:?}", cursor);
                if cursor.1 + 1 < terminal_size.1 {
                    cursor.1 += 1;
                }
                debug!("cursor became: {:?}", cursor);
            }
            (b'', State::Normal) => {
                debug!("Visual bell ^G, ignoring {:?}", cursor);
            }
            /* Backspace */
            (0x08, State::Normal) => {
                //debug!("setting cell {:?} char '{}'", cursor, c as char);
                debug!("backspace x-> x-1, cursor was: {:?}", cursor);
                if cursor.0 > 0 {
                    cursor.0 -= 1;
                }
                //grid[*cursor].set_ch(' ');
                debug!("cursor became: {:?}", cursor);
            }
            (c, State::Normal) => {
                grid[*cursor].set_ch(c as char);
                debug!("setting cell {:?} char '{}'", cursor, c as char);
                increase_cursor_x!();
            }
            (b'u', State::Csi) => {
                /* restore cursor */
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b'm', State::Csi) => {
                /* Character Attributes (SGR).  Ps = 0  -> Normal (default), VT100 */
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b'H', State::Csi) => {
                /*  move cursor to (1,1) */
                debug!("sending {}", EscCode::from((&(*state), byte)),);
                debug!("move cursor to (1,1) cursor before: {:?}", *cursor);
                *cursor = (0, 0);
                debug!("cursor after: {:?}", *cursor);
                *state = State::Normal;
            }
            (b'P', State::Csi) => {
                /*  delete one character */
                debug!("sending {}", EscCode::from((&(*state), byte)),);
                grid[*cursor].set_ch(' ');
                *state = State::Normal;
            }
            (b'C', State::Csi) => {
                // "ESC[C\t\tCSI Cursor Forward one Time",
                debug!("cursor forward one time, cursor was: {:?}", cursor);
                cursor.0 = std::cmp::min(cursor.0 + 1, terminal_size.0.saturating_sub(1));
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            /* CSI ? stuff */
            (c, State::CsiQ(ref mut buf)) if c >= b'0' && c <= b'9' => {
                buf.push(c);
            }
            (c, State::CsiQ(ref mut buf)) => {
                // we are already in AlternativeScreen so do not forward this
                if &buf.as_slice() != &SWITCHALTERNATIVE_1049 {
                    debug!("ignoring {}", EscCode::from((&(*state), byte)));
                }
                *state = State::Normal;
            }
            /* END OF CSI ? stuff */
            /* ******************* */
            /* ******************* */
            /* ******************* */
            (c, State::Csi) if c >= b'0' && c <= b'9' => {
                let mut buf1 = Vec::new();
                buf1.push(c);
                *state = State::Csi1(buf1);
            }
            (b'J', State::Csi) => {
                /* Erase in Display (ED), VT100.*/
                /* Erase Below (default). */
                clear_area(
                    grid,
                    (
                        (
                            0,
                            std::cmp::min(cursor.1 + 1, terminal_size.1.saturating_sub(1)),
                        ),
                        (
                            terminal_size.0.saturating_sub(1),
                            terminal_size.1.saturating_sub(1),
                        ),
                    ),
                );
                debug!("{}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b'K', State::Csi) => {
                /* Erase in Line (ED), VT100.*/
                /* Erase to right (Default) */
                debug!("{}", EscCode::from((&(*state), byte)));
                for x in cursor.0..terminal_size.0 {
                    grid[(x, cursor.1)] = Cell::default();
                }
                *state = State::Normal;
            }
            (c, State::Csi) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b'K', State::Csi1(buf)) if buf == b"0" => {
                /* Erase in Line (ED), VT100.*/
                /* Erase to right (Default) */
                debug!("{}", EscCode::from((&(*state), byte)));
                for x in cursor.0..terminal_size.0 {
                    grid[(x, cursor.1)] = Cell::default();
                }
                *state = State::Normal;
            }
            (b'K', State::Csi1(buf)) if buf == b"1" => {
                /* Erase in Line (ED), VT100.*/
                /* Erase to left (Default) */
                for x in cursor.0..=0 {
                    grid[(x, cursor.1)] = Cell::default();
                }
                debug!("{}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b'K', State::Csi1(buf)) if buf == b"2" => {
                /* Erase in Line (ED), VT100.*/
                /* Erase all */
                for y in 0..terminal_size.1 {
                    for x in 0..terminal_size.0 {
                        grid[(x, y)] = Cell::default();
                    }
                }
                debug!("{}", EscCode::from((&(*state), byte)));
                clear_area(grid, ((0, 0), pos_dec(*terminal_size, (1, 1))));
                *state = State::Normal;
            }
            (b'J', State::Csi1(ref buf)) if buf == b"0" => {
                /* Erase in Display (ED), VT100.*/
                /* Erase Below (default). */
                clear_area(
                    grid,
                    (
                        (
                            0,
                            std::cmp::min(cursor.1 + 1, terminal_size.1.saturating_sub(1)),
                        ),
                        (
                            terminal_size.0.saturating_sub(1),
                            terminal_size.1.saturating_sub(1),
                        ),
                    ),
                );
                debug!("{}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b'J', State::Csi1(ref buf)) if buf == b"1" => {
                /* Erase in Display (ED), VT100.*/
                /* Erase Above */
                clear_area(
                    grid,
                    (
                        (0, 0),
                        (
                            terminal_size.0.saturating_sub(1),
                            cursor.1.saturating_sub(1),
                        ),
                    ),
                );
                debug!("{}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b'J', State::Csi1(ref buf)) if buf == b"2" => {
                /* Erase in Display (ED), VT100.*/
                /* Erase All */
                clear_area(grid, ((0, 0), pos_dec(*terminal_size, (1, 1))));
                debug!("{}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b'J', State::Csi1(ref buf)) if buf == b"3" => {
                /* Erase in Display (ED), VT100.*/
                /* Erase saved lines (What?) */
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b't', State::Csi1(buf)) => {
                /* Window manipulation */
                if buf == b"18" {
                    debug!("report size of the text area");
                    debug!("got {}", EscCode::from((&(*state), byte)));
                    // P s = 1 8 → Report the size of the text area in characters as CSI 8 ; height ; width t
                    stdin.write_all(b"\x1b[8;").unwrap();
                    stdin
                        .write_all((terminal_size.1).to_string().as_bytes())
                        .unwrap();
                    stdin.write_all(&[b';']).unwrap();
                    stdin
                        .write_all((terminal_size.0).to_string().as_bytes())
                        .unwrap();
                    stdin.write_all(&[b't']).unwrap();
                } else {
                    debug!("not sending {}", EscCode::from((&(*state), byte)));
                }
                *state = State::Normal;
            }
            (b'n', State::Csi1(_)) => {
                /* report cursor position */
                debug!("report cursor position");
                debug!("got {}", EscCode::from((&(*state), byte)));
                stdin.write_all(&[b'\x1b', b'[']).unwrap();
                //    Ps = 6  ⇒  Report Cursor Position (CPR) [row;column].
                //Result is CSI r ; c R
                stdin
                    .write_all((cursor.1 + 1).to_string().as_bytes())
                    .unwrap();
                stdin.write_all(&[b';']).unwrap();
                stdin
                    .write_all((cursor.0 + 1).to_string().as_bytes())
                    .unwrap();
                stdin.write_all(&[b'R']).unwrap();
                *state = State::Normal;
            }
            (b'B', State::Csi1(buf)) => {
                //"ESC[{buf}B\t\tCSI Cursor Down {buf} Times",
                let offset = unsafe { std::str::from_utf8_unchecked(buf) }
                    .parse::<usize>()
                    .unwrap();
                debug!("cursor down {} times, cursor was: {:?}", offset, cursor);
                if offset + cursor.1 < terminal_size.1 {
                    cursor.1 += offset;
                }
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            (b'C', State::Csi1(buf)) => {
                // "ESC[{buf}C\t\tCSI Cursor Forward {buf} Times",
                let offset = unsafe { std::str::from_utf8_unchecked(buf) }
                    .parse::<usize>()
                    .unwrap();
                debug!("cursor forward {} times, cursor was: {:?}", offset, cursor);
                if offset + cursor.0 < terminal_size.0 {
                    cursor.0 += offset;
                }
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            (b'D', State::Csi1(buf)) => {
                // "ESC[{buf}D\t\tCSI Cursor Backward {buf} Times",
                let offset = unsafe { std::str::from_utf8_unchecked(buf) }
                    .parse::<usize>()
                    .unwrap();
                debug!("cursor backward {} times, cursor was: {:?}", offset, cursor);
                if offset + cursor.0 < terminal_size.0 {
                    cursor.0 += offset;
                }
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            (b'E', State::Csi1(buf)) => {
                //"ESC[{buf}E\t\tCSI Cursor Next Line {buf} Times",
                let offset = unsafe { std::str::from_utf8_unchecked(buf) }
                    .parse::<usize>()
                    .unwrap();
                debug!(
                    "cursor next line {} times, cursor was: {:?}",
                    offset, cursor
                );
                if offset + cursor.1 < terminal_size.1 {
                    cursor.1 += offset;
                    //cursor.0 = 0;
                }
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            (b'G', State::Csi1(buf)) => {
                // "ESC[{buf}G\t\tCursor Character Absolute  [column={buf}] (default = [row,1])",
                let new_col = unsafe { std::str::from_utf8_unchecked(buf) }
                    .parse::<usize>()
                    .unwrap();
                debug!("cursor absolute {}, cursor was: {:?}", new_col, cursor);
                if new_col < terminal_size.0 + 1 {
                    cursor.0 = new_col.saturating_sub(1);
                } else {
                    debug!(
                        "error: new_cal = {} > terminal.size.0 = {}\nterminal_size = {:?}",
                        new_col, terminal_size.0, terminal_size
                    );
                }
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            (b'C', State::Csi1(buf)) => {
                // "ESC[{buf}C\t\tCSI Cursor Preceding Line {buf} Times",
                let offset = unsafe { std::str::from_utf8_unchecked(buf) }
                    .parse::<usize>()
                    .unwrap();
                debug!(
                    "cursor preceding {} times, cursor was: {:?}",
                    offset, cursor
                );
                if cursor.1 >= offset {
                    cursor.1 -= offset;
                    //cursor.0 = 0;
                }
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            (b'P', State::Csi1(buf)) => {
                // "ESC[{buf}P\t\tCSI Delete {buf} characters, default = 1",
                let offset = unsafe { std::str::from_utf8_unchecked(buf) }
                    .parse::<usize>()
                    .unwrap();
                debug!(
                    "Delete {} Character(s) with cursor at {:?}  ",
                    offset, cursor
                );
                for x in (cursor.0 - std::cmp::min(offset, cursor.0))..cursor.0 {
                    grid[(x, cursor.1)].set_ch(' ');
                }
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            /* CSI Pm d Line Position Absolute [row] (default = [1,column]) (VPA). */
            (b'd', State::Csi1(buf)) => {
                let row = unsafe { std::str::from_utf8_unchecked(buf) }
                    .parse::<usize>()
                    .unwrap();
                debug!(
                    "Line position absolute row {} with cursor at {:?}",
                    row, cursor
                );
                cursor.1 = std::cmp::min(row.saturating_sub(1), terminal_size.1.saturating_sub(1));
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            (b';', State::Csi1(ref mut buf1_p)) => {
                let buf1 = std::mem::replace(buf1_p, Vec::new());
                let buf2 = Vec::new();
                *state = State::Csi2(buf1, buf2);
            }
            (c, State::Csi1(ref mut buf)) if (c >= b'0' && c <= b'9') || c == b' ' => {
                buf.push(c);
            }
            (c, State::Csi1(ref buf)) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b';', State::Csi2(ref mut buf1_p, ref mut buf2_p)) => {
                let buf1 = std::mem::replace(buf1_p, Vec::new());
                let buf2 = std::mem::replace(buf2_p, Vec::new());
                let buf3 = Vec::new();
                *state = State::Csi3(buf1, buf2, buf3);
            }
            (b'n', State::Csi2(_, _)) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                // Report Cursor Position, skip it
                *state = State::Normal;
            }
            (b't', State::Csi2(_, _)) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                // Window manipulation, skip it
                *state = State::Normal;
            }
            (b'H', State::Csi2(ref y, ref x)) => {
                //Cursor Position [row;column] (default = [1,1]) (CUP).
                let orig_x = unsafe { std::str::from_utf8_unchecked(x) }
                    .parse::<usize>()
                    .unwrap_or(1);
                let orig_y = unsafe { std::str::from_utf8_unchecked(y) }
                    .parse::<usize>()
                    .unwrap_or(1);
                debug!("sending {}", EscCode::from((&(*state), byte)),);
                debug!(
                    "cursor set to ({},{}), cursor was: {:?}",
                    orig_x, orig_y, cursor
                );
                if orig_x - 1 < terminal_size.0 && orig_y - 1 < terminal_size.1 {
                    cursor.0 = orig_x - 1;
                    cursor.1 = orig_y - 1;
                } else {
                    debug!(
                        "[error] terminal_size = {:?}, cursor = {:?} but given [{},{}]",
                        terminal_size, cursor, orig_x, orig_y
                    );
                }
                debug!("cursor became: {:?}", cursor);
                *state = State::Normal;
            }
            (c, State::Csi2(_, ref mut buf)) if c >= b'0' && c <= b'9' => {
                buf.push(c);
            }
            (c, State::Csi2(ref buf1, ref buf2)) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b't', State::Csi3(_, _, _)) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                // Window manipulation, skip it
                *state = State::Normal;
            }

            (c, State::Csi3(_, _, ref mut buf)) if c >= b'0' && c <= b'9' => {
                buf.push(c);
            }
            (c, State::Csi3(_, _, _)) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            /* other stuff */
            /* ******************* */
            /* ******************* */
            /* ******************* */
            (c, State::G0) => {
                debug!("ignoring {}", EscCode::from((&(*state), byte)));
                *state = State::Normal;
            }
            (b, s) => {
                debug!("unrecognised: byte is {} and state is {:?}", b as char, s);
            }
        }
    }
}
