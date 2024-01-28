mod cli;
mod command;
mod diff;
mod git;
mod items;
mod keybinds;
mod process;
mod screen;
mod status;
mod theme;
mod ui;

use clap::Parser;
use command::IssuedCommand;
use crossterm::{
    event::{self, Event, KeyEventKind},
    terminal::{
        self, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    },
    ExecutableCommand,
};
use diff::Hunk;
use items::{Item, TargetData};
use keybinds::{Op, TargetOp, TransientOp};
use ratatui::prelude::CrosstermBackend;
use screen::Screen;
use std::{
    io::{self, stderr, BufWriter, Stderr},
    process::Command,
};

type Terminal = ratatui::Terminal<CrosstermBackend<BufWriter<Stderr>>>;

lazy_static::lazy_static! {
    static ref USE_DELTA: bool = Command::new("delta").output().map(|out| out.status.success()).unwrap_or(false);
}

struct State {
    quit: bool,
    screens: Vec<Screen>,
    pending_transient_op: TransientOp,
    pub(crate) command: Option<IssuedCommand>,
}

impl State {
    fn screen_mut(&mut self) -> &mut Screen {
        self.screens.last_mut().expect("No screen")
    }

    fn screen(&self) -> &Screen {
        self.screens.last().expect("No screen")
    }

    pub(crate) fn issue_command(
        &mut self,
        input: &[u8],
        command: Command,
    ) -> Result<(), io::Error> {
        if !self.command.as_mut().is_some_and(|cmd| cmd.is_running()) {
            self.command = Some(IssuedCommand::spawn(input, command)?);
        }

        Ok(())
    }

    pub(crate) fn issue_subscreen_command(
        &mut self,
        terminal: &mut Terminal,
        command: Command,
    ) -> Result<(), io::Error> {
        if !self.command.as_mut().is_some_and(|cmd| cmd.is_running()) {
            self.command = Some(IssuedCommand::spawn_in_subscreen(terminal, command)?);
        }

        Ok(())
    }

    pub(crate) fn clear_finished_command(&mut self) {
        if let Some(ref mut command) = self.command {
            if !command.is_running() {
                self.command = None
            }
        }
    }

    pub(crate) fn handle_command_output(&mut self) {
        if let Some(cmd) = &mut self.command {
            cmd.read_command_output_to_buffer();

            if cmd.just_finished() {
                self.screen_mut().update();
            }
        }
    }
}

fn main() -> io::Result<()> {
    let mut state = create_initial_state(cli::Cli::parse())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::new(stderr())))?;

    terminal.hide_cursor()?;

    enable_raw_mode()?;
    stderr().execute(EnterAlternateScreen)?;

    run_app(&mut terminal, &mut state)?;

    stderr().execute(LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

fn create_initial_state(args: cli::Cli) -> io::Result<State> {
    let size = terminal::size()?;
    let screens = match args.command {
        Some(cli::Commands::Show { git_show_args }) => {
            vec![Screen::new(
                size,
                Box::new(move || {
                    screen::show::create(
                        &git_show_args.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
                    )
                }),
            )]
        }
        Some(cli::Commands::Log { git_log_args }) => {
            vec![Screen::new(
                size,
                Box::new(move || {
                    screen::log::create(
                        &git_log_args.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
                    )
                }),
            )]
        }
        Some(cli::Commands::Diff { git_diff_args }) => {
            vec![Screen::new(
                size,
                Box::new(move || {
                    screen::diff::create(
                        &git_diff_args.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
                    )
                }),
            )]
        }
        None => vec![Screen::new(size, Box::new(screen::status::create))],
    };

    Ok(State {
        quit: false,
        screens,
        pending_transient_op: TransientOp::None,
        command: None,
    })
}

fn run_app(terminal: &mut Terminal, state: &mut State) -> Result<(), io::Error> {
    while !state.quit {
        if let Some(_screen) = state.screens.last_mut() {
            terminal.draw(|frame| ui::ui(frame, state))?;

            state.handle_command_output();
        }

        handle_events(terminal, state)?;

        if let Some(screen) = state.screens.last_mut() {
            screen.clamp_cursor();
        }
    }

    Ok(())
}

fn handle_events(terminal: &mut Terminal, state: &mut State) -> io::Result<()> {
    // TODO Won't need to poll all the time if command outputs were handled async
    if !event::poll(std::time::Duration::from_millis(100))? {
        return Ok(());
    }

    let Some(screen) = state.screens.last_mut() else {
        panic!("No screen");
    };

    match event::read()? {
        Event::Resize(w, h) => screen.size = (w, h),
        Event::Key(key) => {
            if key.kind == KeyEventKind::Press {
                state.clear_finished_command();

                handle_op(terminal, state, key)?;
            }
        }
        _ => (),
    }

    Ok(())
}

fn handle_op(
    terminal: &mut Terminal,
    state: &mut State,
    key: event::KeyEvent,
) -> Result<(), io::Error> {
    let pending = if state.pending_transient_op == TransientOp::Help {
        TransientOp::None
    } else {
        state.pending_transient_op
    };

    if let Some(op) = keybinds::op_of_key_event(pending, key) {
        use Op::*;
        let was_transient = state.pending_transient_op != TransientOp::None;
        state.pending_transient_op = TransientOp::None;

        match op {
            Quit => {
                if was_transient {
                    // Do nothing, already cleared
                } else {
                    state.screens.pop();
                    if let Some(screen) = state.screens.last_mut() {
                        screen.update();
                    } else {
                        state.quit = true
                    }
                }
            }
            Refresh => state.screen_mut().update(),
            ToggleSection => state.screen_mut().toggle_section(),
            SelectPrevious => state.screen_mut().select_previous(),
            SelectNext => state.screen_mut().select_next(),
            HalfPageUp => state.screen_mut().scroll_half_page_up(),
            HalfPageDown => state.screen_mut().scroll_half_page_down(),
            Commit => {
                state.issue_subscreen_command(terminal, git::commit_cmd())?;
                state.screen_mut().update();
            }
            Transient(op) => state.pending_transient_op = op,
            LogCurrent => goto_log_screen(&mut state.screens),
            FetchAll => {
                state.issue_command(&[], git::fetch_all_cmd())?;
                state.screen_mut().update();
            }
            PullRemote => state.issue_command(&[], git::pull_cmd())?,
            PushRemote => state.issue_command(&[], git::push_cmd())?,
            Target(target_op) => {
                if let Some(act) = &state.screen_mut().get_selected_item().target_data.clone() {
                    if let Some(mut closure) = closure_by_target_op(act, &target_op) {
                        closure(terminal, state);
                    }
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn list_target_ops<'a>(
    target: &'a TargetData,
) -> impl Iterator<Item = &'static TargetOp> + 'a {
    TargetOp::list_all().filter(|target_op| closure_by_target_op(target, target_op).is_some())
}

type OpClosure<'a> = Box<dyn FnMut(&mut Terminal, &mut State) + 'a>;

pub(crate) fn closure_by_target_op<'a>(
    target: &'a TargetData,
    target_op: &TargetOp,
) -> Option<OpClosure<'a>> {
    match (target_op, target) {
        (TargetOp::Show, TargetData::Ref(r)) => Some(Box::new(move |_terminal, state| {
            goto_show_screen(r, &mut state.screens);
        })),
        (TargetOp::Show, TargetData::File(u)) => {
            let untracked = u.clone();
            Some(Box::new(move |terminal, state| {
                state
                    .issue_subscreen_command(terminal, editor_cmd(&untracked, None))
                    .expect("Error opening editor");
                state.screen_mut().update();
            }))
        }
        (TargetOp::Show, TargetData::Delta(d)) => {
            let delta = d.clone();
            Some(Box::new(move |terminal, state| {
                state
                    .issue_subscreen_command(terminal, editor_cmd(&delta.new_file, None))
                    .expect("Error opening editor");
                state.screen_mut().update();
            }))
        }
        (TargetOp::Show, TargetData::Hunk(h)) => {
            let hunk = h.clone();
            Some(Box::new(move |terminal, state| {
                state
                    .issue_subscreen_command(terminal, editor_cmd(&hunk.new_file, Some(&hunk)))
                    .expect("Error opening editor");
                state.screen_mut().update();
            }))
        }
        (TargetOp::Stage, TargetData::Ref(_)) => None,
        (TargetOp::Stage, TargetData::File(u)) => {
            let untracked = u.clone();
            Some(Box::new(move |_terminal, state| {
                state
                    .issue_command(&[], git::stage_file_cmd(&untracked))
                    .expect("Error staging file");
                state.screen_mut().update();
            }))
        }
        (TargetOp::Stage, TargetData::Delta(d)) => {
            let delta = d.clone();
            Some(Box::new(move |_terminal, state| {
                state
                    .issue_command(&[], git::stage_file_cmd(&delta.new_file))
                    .expect("Error staging file");
                state.screen_mut().update();
            }))
        }
        (TargetOp::Stage, TargetData::Hunk(h)) => {
            let hunk = h.clone();
            Some(Box::new(move |_terminal, state| {
                state
                    .issue_command(hunk.format_patch().as_bytes(), git::stage_patch_cmd())
                    .expect("Error staging hunk");
                state.screen_mut().update();
            }))
        }
        (TargetOp::Unstage, TargetData::Ref(_)) => None,
        (TargetOp::Unstage, TargetData::File(_)) => None,
        (TargetOp::Unstage, TargetData::Delta(d)) => {
            let delta = d.clone();
            Some(Box::new(move |_terminal, state| {
                state
                    .issue_command(&[], git::unstage_file_cmd(&delta))
                    .expect("Error unstaging file");
                state.screen_mut().update();
            }))
        }
        (TargetOp::Unstage, TargetData::Hunk(h)) => {
            let hunk = h.clone();
            Some(Box::new(move |_terminal, state| {
                state
                    .issue_command(hunk.format_patch().as_bytes(), git::unstage_patch_cmd())
                    .expect("Error unstaging hunk");
                state.screen_mut().update();
            }))
        }
        (TargetOp::CopyToClipboard, TargetData::Ref(r)) => {
            let reference = r.clone();
            Some(Box::new(move |_terminal, _state| {
                cli_clipboard::set_contents(reference.to_string())
                    .expect("Couldn't write to clipboard")
            }))
        }
        (TargetOp::CopyToClipboard, TargetData::File(u)) => {
            let untracked = u.clone();
            Some(Box::new(move |_terminal, _state| {
                cli_clipboard::set_contents(untracked.clone()).expect("Couldn't write to clipboard")
            }))
        }
        (TargetOp::CopyToClipboard, TargetData::Delta(d)) => {
            let file = d.new_file.clone();
            Some(Box::new(move |_terminal, _state| {
                cli_clipboard::set_contents(file.clone()).expect("Couldn't write to clipboard")
            }))
        }
        (TargetOp::CopyToClipboard, TargetData::Hunk(h)) => {
            let patch = h.format_patch();
            Some(Box::new(move |_terminal, _state| {
                cli_clipboard::set_contents(patch.clone()).expect("Couldn't write to clipboard")
            }))
        }
        (TargetOp::RebaseInteractive, TargetData::Ref(r)) => {
            Some(Box::new(move |terminal, state| {
                state
                    .issue_subscreen_command(terminal, git::rebase_interactive_cmd(r))
                    .expect("Error rebasing");
                state.screen_mut().update();
            }))
        }
        (TargetOp::RebaseInteractive, TargetData::File(_)) => None,
        (TargetOp::RebaseInteractive, TargetData::Delta(_)) => None,
        (TargetOp::RebaseInteractive, TargetData::Hunk(_)) => None,
    }
}

fn goto_show_screen(reference: &str, screens: &mut Vec<Screen>) {
    let size = terminal::size().expect("Error reading terminal size");
    let ref_clone = reference.to_string();
    screens.push(Screen::new(
        size,
        Box::new(move || screen::show::create(&[&ref_clone])),
    ));
}

fn goto_log_screen(screens: &mut Vec<Screen>) {
    let size = terminal::size().expect("Error reading terminal size");
    screens.drain(1..);
    screens.push(Screen::new(size, Box::new(|| screen::log::create(&[]))));
}

fn editor_cmd(delta: &str, maybe_hunk: Option<&Hunk>) -> Command {
    let editor = std::env::var("EDITOR").expect("EDITOR not set");
    let mut cmd = Command::new(editor.clone());
    let args = match maybe_hunk {
        Some(hunk) => match editor.as_str() {
            "vi" | "vim" | "nvim" | "nano" => {
                vec![format!("+{}", hunk.new_start), delta.to_string()]
            }
            _ => vec![format!("{}:{}", delta, hunk.new_start)],
        },
        None => vec![delta.to_string()],
    };

    cmd.args(args);
    cmd
}
