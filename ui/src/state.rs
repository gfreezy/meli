/*
 * meli - ui crate.
 *
 * Copyright 2017-2018 Manos Pitsidianakis
 *
 * This file is part of meli.
 *
 * meli is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * meli is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with meli. If not, see <http://www.gnu.org/licenses/>.
 */

/*! The application's state.

The UI crate has an Box<dyn Component>-Component-System design. The System part, is also the application's state, so they're both merged in the `State` struct.

`State` owns all the Components of the UI. In the application's main event loop, input is handed to the state in the form of `UIEvent` objects which traverse the component graph. Components decide to handle each input or not.

Input is received in the main loop from threads which listen on the stdin for user input, observe folders for file changes etc. The relevant struct is `ThreadEvent`.
*/

use super::*;
use melib::backends::{FolderHash, NotifyFn};

use crossbeam::channel::{bounded, unbounded, Receiver, Sender};
use fnv::FnvHashMap;
use std::env;
use std::io::Write;
use std::result;
use std::thread;
use termion::raw::IntoRawMode;
use termion::screen::AlternateScreen;
use termion::{clear, cursor};

pub type StateStdout = termion::screen::AlternateScreen<termion::raw::RawTerminal<std::io::Stdout>>;

struct InputHandler {
    rx: Receiver<InputCommand>,
    tx: Sender<InputCommand>,
}

impl InputHandler {
    fn restore(&self, tx: Sender<ThreadEvent>) {
        let rx = self.rx.clone();
        thread::Builder::new()
            .name("input-thread".to_string())
            .spawn(move || {
                get_events(
                    |k| {
                        tx.send(ThreadEvent::Input(k)).unwrap();
                    },
                    |i| {
                        tx.send(ThreadEvent::InputRaw(i)).unwrap();
                    },
                    &rx,
                )
            })
            .unwrap();
    }

    fn kill(&self) {
        self.tx.send(InputCommand::Kill).unwrap();
    }

    fn switch_to_raw(&self) {
        self.tx.send(InputCommand::Raw).unwrap();
    }

    fn switch_from_raw(&self) {
        self.tx.send(InputCommand::NoRaw).unwrap();
    }
}

/// A context container for loaded settings, accounts, UI changes, etc.
pub struct Context {
    pub accounts: Vec<Account>,
    pub mailbox_hashes: FnvHashMap<FolderHash, usize>,
    pub settings: Settings,

    pub runtime_settings: Settings,
    /// Areas of the screen that must be redrawn in the next render
    pub dirty_areas: VecDeque<Area>,

    /// Events queue that components send back to the state
    pub replies: VecDeque<UIEvent>,
    sender: Sender<ThreadEvent>,
    receiver: Receiver<ThreadEvent>,
    input: InputHandler,
    work_controller: WorkController,

    pub temp_files: Vec<File>,
}

impl Context {
    pub fn replies(&mut self) -> Vec<UIEvent> {
        self.replies.drain(0..).collect()
    }

    pub fn input_kill(&self) {
        self.input.kill();
    }

    pub fn input_from_raw(&self) {
        self.input.switch_from_raw();
    }

    pub fn input_to_raw(&self) {
        self.input.switch_to_raw();
    }

    pub fn restore_input(&self) {
        self.input.restore(self.sender.clone());
    }
    pub fn account_status(
        &mut self,
        idx_a: usize,
        folder_hash: FolderHash,
    ) -> result::Result<(), usize> {
        match self.accounts[idx_a].status(folder_hash) {
            Ok(()) => {
                self.replies
                    .push_back(UIEvent::MailboxUpdate((idx_a, folder_hash)));
                Ok(())
            }
            Err(n) => Err(n),
        }
    }

    pub fn work_controller(&self) -> &WorkController {
        &self.work_controller
    }
}

/// A State object to manage and own components and components of the UI. `State` is responsible for
/// managing the terminal and interfacing with `melib`
pub struct State {
    cols: usize,
    rows: usize,

    grid: CellBuffer,
    stdout: Option<StateStdout>,
    child: Option<ForkType>,
    pub mode: UIMode,
    components: Vec<Box<dyn Component>>,
    pub context: Context,
    threads: FnvHashMap<thread::ThreadId, (Sender<bool>, thread::JoinHandle<()>)>,
}

impl Drop for State {
    fn drop(&mut self) {
        // When done, restore the defaults to avoid messing with the terminal.
        self.switch_to_main_screen();
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        /* Create a channel to communicate with other threads. The main process is the sole receiver.
         * */
        let (sender, receiver) = bounded(32 * ::std::mem::size_of::<ThreadEvent>());

        /*
         * Create async channel to block the input-thread if we need to fork and stop it from reading
         * stdin, see get_events() for details
         * */
        let input_thread = unbounded();
        let backends = Backends::new();
        let settings = Settings::new();

        let termsize = termion::terminal_size().ok();
        let termcols = termsize.map(|(w, _)| w);
        let termrows = termsize.map(|(_, h)| h);
        let cols = termcols.unwrap_or(0) as usize;
        let rows = termrows.unwrap_or(0) as usize;

        let work_controller = WorkController::new(sender.clone());
        let mut accounts: Vec<Account> = settings
            .accounts
            .iter()
            .enumerate()
            .map(|(index, (n, a_s))| {
                let sender = sender.clone();
                Account::new(
                    index,
                    n.to_string(),
                    a_s.clone(),
                    &backends,
                    work_controller.get_context(),
                    NotifyFn::new(Box::new(move |f: FolderHash| {
                        sender
                            .send(ThreadEvent::UIEvent(UIEvent::StartupCheck(f)))
                            .unwrap();
                    })),
                )
            })
            .collect();
        accounts.sort_by(|a, b| a.name().cmp(&b.name()));

        let mut s = State {
            cols,
            rows,
            grid: CellBuffer::new(cols, rows, Cell::with_char(' ')),
            stdout: None,
            child: None,
            mode: UIMode::Normal,
            components: Vec::with_capacity(1),

            context: Context {
                accounts,
                mailbox_hashes: FnvHashMap::with_capacity_and_hasher(1, Default::default()),

                settings: settings.clone(),
                runtime_settings: settings,
                dirty_areas: VecDeque::with_capacity(5),
                replies: VecDeque::with_capacity(5),
                temp_files: Vec::new(),
                work_controller,

                sender,
                receiver,
                input: InputHandler {
                    rx: input_thread.1,
                    tx: input_thread.0,
                },
            },
            threads: FnvHashMap::with_capacity_and_hasher(1, Default::default()),
        };
        if s.context.settings.terminal.ascii_drawing {
            s.grid.set_ascii_drawing(true);
        }

        s.switch_to_alternate_screen();
        debug!("inserting mailbox hashes:");
        {
            /* Account::watch() needs
             * - work_controller to pass `work_context` to the watcher threads and then add them
             *   to the controller's static thread list,
             * - sender to pass a RefreshEventConsumer closure to watcher threads for them to
             *   inform the main binary that refresh events arrived
             * - replies to report any failures to the user
             */
            let Context {
                ref mut work_controller,
                ref sender,
                ref mut replies,
                ref mut accounts,
                ref mut mailbox_hashes,
                ..
            } = &mut s.context;

            for (x, account) in accounts.iter_mut().enumerate() {
                for folder in account.backend.folders().values() {
                    debug!("hash & folder: {:?} {}", folder.hash(), folder.name());
                    mailbox_hashes.insert(folder.hash(), x);
                }
                account.watch((work_controller, sender, replies));
            }
        }
        s.context.restore_input();
        s
    }

    /*
     * When we receive a folder hash from a watcher thread,
     * we match the hash to the index of the mailbox, request a reload
     * and startup a thread to remind us to poll it every now and then till it's finished.
     */
    pub fn refresh_event(&mut self, event: RefreshEvent) {
        let hash = event.hash();
        if let Some(&idxa) = self.context.mailbox_hashes.get(&hash) {
            if self.context.accounts[idxa].status(hash).is_err() {
                self.context.replies.push_back(UIEvent::from(event));
                return;
            }
            let Context {
                ref mut work_controller,
                ref sender,
                ref mut replies,
                ref mut accounts,
                ..
            } = &mut self.context;

            if let Some(notification) =
                accounts[idxa].reload(event, hash, (work_controller, sender, replies))
            {
                if let UIEvent::Notification(_, _, _) = notification {
                    self.rcv_event(UIEvent::MailboxUpdate((idxa, hash)));
                }
                self.rcv_event(notification);
            }
        } else {
            debug!(
                "BUG: mailbox with hash {} not found in mailbox_hashes.",
                hash
            );
        }
    }

    /// If an owned thread returns a `ThreadEvent::ThreadJoin` event to `State` then it must remove
    /// the thread from its list and `join` it.
    pub fn join(&mut self, id: thread::ThreadId) {
        let (tx, handle) = self.threads.remove(&id).unwrap();
        tx.send(true).unwrap();
        handle.join().unwrap();
    }

    /// Switch back to the terminal's main screen (The command line the user sees before opening
    /// the application)
    pub fn switch_to_main_screen(&mut self) {
        write!(
            self.stdout(),
            "{}{}{}{}",
            termion::screen::ToMainScreen,
            cursor::Show,
            RestoreWindowTitleIconFromStack,
            BracketModeEnd,
        )
        .unwrap();
        self.flush();
        self.stdout = None;
        self.context.input.kill();
    }

    pub fn switch_to_alternate_screen(&mut self) {
        let s = std::io::stdout();

        let mut stdout = AlternateScreen::from(s.into_raw_mode().unwrap());

        write!(
            &mut stdout,
            "{save_title_to_stack}{}{}{}{window_title}{}{}",
            termion::screen::ToAlternateScreen,
            cursor::Hide,
            clear::All,
            cursor::Goto(1, 1),
            BracketModeStart,
            save_title_to_stack = SaveWindowTitleIconToStack,
            window_title = if let Some(ref title) = self.context.settings.terminal.window_title {
                format!("\x1b]2;{}\x07", title)
            } else {
                String::new()
            },
        )
        .unwrap();

        self.stdout = Some(stdout);
        self.flush();
    }

    pub fn receiver(&self) -> Receiver<ThreadEvent> {
        self.context.receiver.clone()
    }

    pub fn sender(&self) -> Sender<ThreadEvent> {
        self.context.sender.clone()
    }

    pub fn restore_input(&mut self) {
        self.context.restore_input();
    }

    /// On `SIGWNICH` the `State` redraws itself according to the new terminal size.
    pub fn update_size(&mut self) {
        let termsize = termion::terminal_size().ok();
        let termcols = termsize.map(|(w, _)| w);
        let termrows = termsize.map(|(_, h)| h);
        if termcols.unwrap_or(72) as usize != self.cols
            || termrows.unwrap_or(120) as usize != self.rows
        {
            debug!(
                "Size updated, from ({}, {}) -> ({:?}, {:?})",
                self.cols, self.rows, termcols, termrows
            );
        }
        self.cols = termcols.unwrap_or(72) as usize;
        self.rows = termrows.unwrap_or(120) as usize;
        self.grid.resize(self.cols, self.rows, Cell::with_char(' '));

        self.rcv_event(UIEvent::Resize);

        // Invalidate dirty areas.
        self.context.dirty_areas.clear();
    }

    /// Force a redraw for all dirty components.
    pub fn redraw(&mut self) {
        for i in 0..self.components.len() {
            self.draw_component(i);
        }
        let mut areas: Vec<Area> = self.context.dirty_areas.drain(0..).collect();
        if areas.is_empty() {
            return;
        }
        /* Sort by x_start, ie upper_left corner's x coordinate */
        areas.sort_by(|a, b| (a.0).0.partial_cmp(&(b.0).0).unwrap());
        /* draw each dirty area */
        let rows = self.rows;
        for y in 0..rows {
            let mut segment = None;
            for ((x_start, y_start), (x_end, y_end)) in &areas {
                if y < *y_start || y > *y_end {
                    continue;
                }
                if let Some((x_start, x_end)) = segment.take() {
                    self.draw_horizontal_segment(x_start, x_end, y);
                }
                match segment {
                    ref mut s @ None => {
                        *s = Some((*x_start, *x_end));
                    }
                    ref mut s @ Some(_) if s.unwrap().1 < *x_start => {
                        self.draw_horizontal_segment(s.unwrap().0, s.unwrap().1, y);
                        *s = Some((*x_start, *x_end));
                    }
                    ref mut s @ Some(_) if s.unwrap().1 < *x_end => {
                        self.draw_horizontal_segment(s.unwrap().0, s.unwrap().1, y);
                        *s = Some((s.unwrap().1, *x_end));
                    }
                    Some((_, ref mut x)) => {
                        *x = *x_end;
                    }
                }
            }
            if let Some((x_start, x_end)) = segment {
                self.draw_horizontal_segment(x_start, x_end, y);
            }
        }
        self.flush();
    }

    /// Draw only a specific `area` on the screen.
    fn draw_horizontal_segment(&mut self, x_start: usize, x_end: usize, y: usize) {
        write!(
            self.stdout(),
            "{}",
            cursor::Goto(x_start as u16 + 1, (y + 1) as u16)
        )
        .unwrap();
        for x in x_start..=x_end {
            let c = self.grid[(x, y)];
            if c.bg() != Color::Default {
                c.bg().write_bg(self.stdout()).unwrap();
            }
            if c.fg() != Color::Default {
                c.fg().write_fg(self.stdout()).unwrap();
            }
            if c.attrs() != Attr::Default {
                write!(self.stdout(), "\x1B[{}m", c.attrs() as u8).unwrap();
            }
            if !c.empty() {
                write!(self.stdout(), "{}", c.ch()).unwrap();
            }

            if c.bg() != Color::Default {
                write!(
                    self.stdout(),
                    "{}",
                    termion::color::Bg(termion::color::Reset)
                )
                .unwrap();
            }
            if c.fg() != Color::Default {
                write!(
                    self.stdout(),
                    "{}",
                    termion::color::Fg(termion::color::Reset)
                )
                .unwrap();
            }
            if c.attrs() != Attr::Default {
                write!(self.stdout(), "\x1B[{}m", Attr::Default as u8).unwrap();
            }
        }
    }

    /// Draw the entire screen from scratch.
    pub fn render(&mut self) {
        self.update_size();
        let cols = self.cols;
        let rows = self.rows;
        self.context
            .dirty_areas
            .push_back(((0, 0), (cols - 1, rows - 1)));

        self.redraw();
    }

    pub fn draw_component(&mut self, idx: usize) {
        let component = &mut self.components[idx];
        let upper_left = (0, 0);
        let bottom_right = (self.cols - 1, self.rows - 1);

        if component.is_dirty() {
            component.draw(
                &mut self.grid,
                (upper_left, bottom_right),
                &mut self.context,
            );
        }
    }

    pub fn can_quit_cleanly(&mut self) -> bool {
        let State {
            ref mut components,
            ref context,
            ..
        } = self;
        components.iter_mut().all(|c| c.can_quit_cleanly(context))
    }

    pub fn register_component(&mut self, component: Box<dyn Component>) {
        self.components.push(component);
    }

    /// Convert user commands to actions/method calls.
    fn parse_command(&mut self, cmd: &str) {
        let result = parse_command(&cmd.as_bytes()).to_full_result();

        if let Ok(v) = result {
            match v {
                SetEnv(key, val) => {
                    env::set_var(key.as_str(), val.as_str());
                }
                PrintEnv(key) => {
                    self.context.replies.push_back(UIEvent::StatusEvent(
                        StatusEvent::DisplayMessage(
                            env::var(key.as_str()).unwrap_or_else(|e| e.to_string()),
                        ),
                    ));
                }
                Folder(account_name, path, op) => {
                    if let Some(account) = self
                        .context
                        .accounts
                        .iter_mut()
                        .find(|a| a.name() == account_name)
                    {
                        if let Err(e) = account.folder_operation(&path, op) {
                            self.context.replies.push_back(UIEvent::StatusEvent(
                                StatusEvent::DisplayMessage(e.to_string()),
                            ));
                        }
                    } else {
                        self.context.replies.push_back(UIEvent::StatusEvent(
                            StatusEvent::DisplayMessage(format!(
                                "Account with name `{}` not found.",
                                account_name
                            )),
                        ));
                    }
                }
                v => {
                    self.rcv_event(UIEvent::Action(v));
                }
            }
        } else {
            self.context
                .replies
                .push_back(UIEvent::StatusEvent(StatusEvent::DisplayMessage(
                    "invalid command".to_string(),
                )));
        }
    }

    /// The application's main loop sends `UIEvents` to state via this method.
    pub fn rcv_event(&mut self, mut event: UIEvent) {
        match event {
            // Command type is handled only by State.
            UIEvent::Command(cmd) => {
                self.parse_command(&cmd);
                return;
            }
            UIEvent::Fork(child) => {
                self.mode = UIMode::Fork;
                self.child = Some(child);
                if let Some(ForkType::Finished) = self.child {
                    /*
                     * Fork has finished in the past.
                     * We're back in the AlternateScreen, but the cursor is reset to Shown, so fix
                     * it.
                     */
                    write!(self.stdout(), "{}", cursor::Hide,).unwrap();
                    self.flush();
                }
                return;
            }
            UIEvent::ChangeMode(m) => {
                if self.mode == UIMode::Embed {
                    self.context.input_from_raw();
                }
                self.context
                    .sender
                    .send(ThreadEvent::UIEvent(UIEvent::ChangeMode(m)))
                    .unwrap();
                if m == UIMode::Embed {
                    self.context.input_to_raw();
                }
            }
            _ => {}
        }
        /* inform each component */
        for i in 0..self.components.len() {
            self.components[i].process_event(&mut event, &mut self.context);
        }

        if !self.context.replies.is_empty() {
            let replies: Vec<UIEvent> = self.context.replies.drain(0..).collect();
            // Pass replies to self and call count on the map iterator to force evaluation
            replies.into_iter().map(|r| self.rcv_event(r)).count();
        }
    }

    pub fn try_wait_on_child(&mut self) -> Option<bool> {
        let should_return_flag = match self.child {
            Some(ForkType::NewDraft(_, ref mut c)) => {
                let w = c.try_wait();
                match w {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(e) => {
                        log(
                            format!("Failed to wait on editor process: {}", e.to_string()),
                            ERROR,
                        );
                        return None;
                    }
                }
            }
            Some(ForkType::Generic(ref mut c)) => {
                let w = c.try_wait();
                match w {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(e) => {
                        log(
                            format!("Failed to wait on child process: {}", e.to_string()),
                            ERROR,
                        );
                        return None;
                    }
                }
            }
            Some(ForkType::Finished) => {
                /* Fork has already finished */
                std::mem::replace(&mut self.child, None);
                return None;
            }
            _ => {
                return None;
            }
        };
        if should_return_flag {
            return Some(true);
        }
        Some(false)
    }
    fn flush(&mut self) {
        if let Some(s) = self.stdout.as_mut() {
            s.flush().unwrap();
        }
    }
    fn stdout(&mut self) -> &mut StateStdout {
        self.stdout.as_mut().unwrap()
    }
}
