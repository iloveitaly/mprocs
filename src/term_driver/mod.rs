use std::collections::VecDeque;
use std::io::Write;
use std::pin::Pin;
use std::time::Duration;

mod input;
pub(crate) mod internal;
#[cfg(windows)]
mod windows;

use crate::{
  error::ResultLogger,
  term::{Size, TermEvent, vt::emit},
};

use self::{
  input::EventDecoder,
  internal::{InternalTermEvent, KeyboardMode},
};

pub struct TermDriver {
  #[cfg(unix)]
  stdin_fd: rustix::fd::BorrowedFd<'static>,
  #[cfg(unix)]
  orig_termios: rustix::termios::Termios,
  #[cfg(unix)]
  stdin_thread: Option<std::thread::JoinHandle<()>>,
  #[cfg(unix)]
  stdin_wakeup: std::os::fd::OwnedFd,
  #[cfg(unix)]
  sigwinch: tokio::signal::unix::Signal,

  #[cfg(windows)]
  win_vt: windows::WinVt,

  events:
    tokio::sync::mpsc::UnboundedReceiver<std::io::Result<InternalTermEvent>>,

  stdout: std::io::Stdout,

  pending: VecDeque<InternalTermEvent>,
  init_timeout: Option<Pin<Box<tokio::time::Sleep>>>,
  keyboard: KeyboardMode,
}

impl TermDriver {
  pub fn create() -> anyhow::Result<Self> {
    #[cfg(unix)]
    let stdin_fd = rustix::stdio::stdin();
    #[cfg(unix)]
    if !rustix::termios::isatty(stdin_fd) {
      anyhow::bail!("Stdin is not a tty.");
    }

    #[cfg(windows)]
    let win_vt = windows::WinVt::enable()?;

    let mut stdout = std::io::stdout();

    #[cfg(unix)]
    let orig_termios = rustix::termios::tcgetattr(stdin_fd)?;
    #[cfg(unix)]
    let mut termios = orig_termios.clone();
    #[cfg(unix)]
    termios.make_raw();
    #[cfg(unix)]
    rustix::termios::tcsetattr(
      stdin_fd,
      rustix::termios::OptionalActions::Now,
      &termios,
    )?;

    let mut seq: Vec<u8> = Vec::new();
    seq.extend_from_slice(emit::SAVE_CURSOR.as_bytes());
    emit::dec_set(&mut seq, emit::DecMode::AltScreen);
    seq.extend_from_slice(emit::CLEAR_ALL.as_bytes());
    // Mouse: press/release, drag motion, all motion, and the rxvt/SGR
    // encodings that allow coordinates over 223 (SGR preferred).
    emit::dec_set(&mut seq, emit::DecMode::MousePressRelease);
    emit::dec_set(&mut seq, emit::DecMode::MouseButtonMotion);
    emit::dec_set(&mut seq, emit::DecMode::MouseAnyMotion);
    emit::dec_set(&mut seq, emit::DecMode::MouseRxvt);
    emit::dec_set(&mut seq, emit::DecMode::MouseSgr);
    emit::dec_set(&mut seq, emit::DecMode::BracketedPaste);

    // Query kitty keyboard protocol. Skipped on Windows (issue #215).
    #[cfg(unix)]
    seq.extend_from_slice(emit::KITTY_QUERY.as_bytes());
    // Query device.
    seq.extend_from_slice(emit::DA1_QUERY.as_bytes());
    stdout.write_all(&seq)?;

    #[cfg(unix)]
    let sigwinch = tokio::signal::unix::signal(
      tokio::signal::unix::SignalKind::window_change(),
    )
    .expect("Failed to register SIGWINCH handler");

    #[cfg(unix)]
    let (events, stdin_thread, stdin_wakeup) = {
      use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

      let (sender, events) = tokio::sync::mpsc::unbounded_channel();
      let mut pipe_fds = [0; 2];
      unsafe {
        if libc::pipe(pipe_fds.as_mut_ptr()) < 0 {
          return Err(std::io::Error::last_os_error().into());
        }
      }

      let wake_read = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
      let wake_write = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
      let stdin_raw = stdin_fd.as_raw_fd();

      let stdin_thread = std::thread::spawn(move || {
        let mut decoder = EventDecoder::new();
        let mut read_buf = [0u8; 1024];

        loop {
          let mut poll_fds = [
            libc::pollfd {
              fd: stdin_raw,
              events: libc::POLLIN,
              revents: 0,
            },
            libc::pollfd {
              fd: wake_read.as_raw_fd(),
              events: libc::POLLIN,
              revents: 0,
            },
          ];

          // Note: tty stdin can only be awaited with select/poll on Macos.
          let poll_result = unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, -1) };
          if poll_result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
              continue;
            }
            let _ = sender.send(Err(err));
            break;
          }

          if (poll_fds[1].revents & libc::POLLIN) != 0 {
            break;
          }

          if (poll_fds[0].revents & (libc::POLLIN | libc::POLLHUP)) != 0 {
            let n = match rustix::io::read(stdin_fd, &mut read_buf) {
              Ok(n) => n,
              Err(err) => {
                let io_err = std::io::Error::from(err);
                if io_err.kind() == std::io::ErrorKind::Interrupted {
                  continue;
                }
                let _ = sender.send(Err(io_err));
                break;
              }
            };

            if n == 0 {
              break;
            }

            decoder.feed(&read_buf[..n], |event| {
              let _ = sender.send(Ok(event));
            });
            decoder.flush(|event| {
              let _ = sender.send(Ok(event));
            });
          }
        }
      });
      (events, Some(stdin_thread), wake_write)
    };

    #[cfg(windows)]
    let events = {
      let (sender, events) = tokio::sync::mpsc::unbounded_channel();
      unsafe {
        std::thread::spawn(move || {
          let stdin = match ::windows::Win32::System::Console::GetStdHandle(
            ::windows::Win32::System::Console::STD_INPUT_HANDLE,
          ) {
            Ok(stdin) => stdin,
            Err(err) => {
              log::error!("GetStdHandle error: {}", err);
              return;
            }
          };
          let mut decoder = EventDecoder::new();
          let mut buf =
            [::windows::Win32::System::Console::INPUT_RECORD::default(); 128];
          loop {
            let mut count = 0;
            match ::windows::Win32::System::Console::ReadConsoleInputA(
              stdin, &mut buf, &mut count,
            ) {
              Ok(()) => (),
              Err(err) => {
                log::error!("ReadConsoleInputA error: {}", err);
                break;
              }
            };

            windows::decode_input_records(
              &mut decoder,
              &buf[..count as usize],
              &mut |event| {
                let _ = sender.send(Ok(event));
              },
            );
          }
        });
      };
      events
    };

    Ok(Self {
      #[cfg(unix)]
      stdin_fd,
      #[cfg(unix)]
      orig_termios,
      #[cfg(unix)]
      stdin_thread,
      #[cfg(unix)]
      stdin_wakeup,
      #[cfg(unix)]
      sigwinch,

      #[cfg(windows)]
      win_vt,

      events,

      stdout,
      pending: VecDeque::new(),
      init_timeout: Some(Box::pin(tokio::time::sleep(Duration::from_millis(
        200,
      )))),
      keyboard: KeyboardMode::Unknown,
    })
  }

  fn handle_internal(
    &mut self,
    event: InternalTermEvent,
  ) -> std::io::Result<Option<TermEvent>> {
    match event {
      InternalTermEvent::Key(key_event) => {
        return Ok(Some(TermEvent::Key(key_event)));
      }
      InternalTermEvent::Mouse(mouse_event) => {
        return Ok(Some(TermEvent::Mouse(mouse_event)));
      }
      InternalTermEvent::Paste(text) => {
        return Ok(Some(TermEvent::Paste(text)));
      }
      InternalTermEvent::Resize(cols, rows) => {
        return Ok(Some(TermEvent::Resize(cols, rows)));
      }
      InternalTermEvent::FocusGained => {
        return Ok(Some(TermEvent::FocusGained));
      }
      InternalTermEvent::FocusLost => return Ok(Some(TermEvent::FocusLost)),
      InternalTermEvent::CursorPos(_x, _y) => (),
      InternalTermEvent::PrimaryDeviceAttributes => {
        self.init_timeout = None;
        self.activate_keyboard_fallback()?;
      }
      InternalTermEvent::ReplyKittyKeyboard(_flags) => {
        self.keyboard = KeyboardMode::Kitty;
        // 0b1 (1) - Disambiguate escape codes
        // 0b10 (2) - Report event types
        // 0b100 (4) - Report alternate keys
        // 0b1000 (8) - Report all keys as escape codes
        // 0b10000 (16) - Report associated text
        // 0b1111 = 15
        let mut seq = Vec::new();
        emit::kitty_push(&mut seq, 15);
        self.stdout.write_all(&seq)?;
      }
    };
    Ok(None)
  }

  fn activate_keyboard_fallback(&mut self) -> std::io::Result<()> {
    if matches!(self.keyboard, KeyboardMode::Unknown) {
      let mut seq = Vec::new();
      #[cfg(unix)]
      {
        self.keyboard = KeyboardMode::ModifyOtherKeys;
        emit::modify_other_keys(&mut seq, 2);
      }
      #[cfg(windows)]
      {
        self.keyboard = KeyboardMode::Win32;
        emit::dec_set(&mut seq, emit::DecMode::Win32Input);
      }
      self.stdout.write_all(&seq)?;
    }
    Ok(())
  }

  #[cfg(unix)]
  pub async fn input(&mut self) -> std::io::Result<Option<TermEvent>> {
    loop {
      // Drain buffered events first.
      while let Some(event) = self.pending.pop_front() {
        if let Some(term_event) = self.handle_internal(event)? {
          return Ok(Some(term_event));
        }
      }

      tokio::select! {
        event = self.events.recv() => {
          match event {
            Some(Ok(event)) => {
              if let Some(term_event) = self.handle_internal(event)? {
                return Ok(Some(term_event));
              }
            }
            Some(Err(err)) => return Err(err),
            None => return Ok(None),
          }
        }
        _ = self.sigwinch.recv() => {
          let winsize = rustix::termios::tcgetwinsize(self.stdin_fd)?;
          return Ok(Some(TermEvent::Resize(winsize.ws_col, winsize.ws_row)));
        }
        _ = async {
          match &mut self.init_timeout {
            Some(sleep) => sleep.await,
            None => std::future::pending().await,
          }
        } => {
          self.init_timeout = None;
          self.activate_keyboard_fallback()?;
        }
      }
    }
  }

  #[cfg(windows)]
  pub async fn input(&mut self) -> std::io::Result<Option<TermEvent>> {
    loop {
      // Drain buffered events first.
      while let Some(event) = self.pending.pop_front() {
        if let Some(term_event) = self.handle_internal(event)? {
          return Ok(Some(term_event));
        }
      }

      tokio::select! {
        event = self.events.recv() => {
          match event {
            Some(Ok(event)) => {
              if let Some(term_event) = self.handle_internal(event)? {
                return Ok(Some(term_event));
              }
            }
            Some(Err(err)) => return Err(err),
            None => return Ok(None),
          }
        }
        _ = async {
          match &mut self.init_timeout {
            Some(sleep) => sleep.await,
            None => std::future::pending().await,
          }
        } => {
          self.init_timeout = None;
          self.activate_keyboard_fallback()?;
        }
      }
    }
  }

  #[cfg(unix)]
  pub fn size(&self) -> std::io::Result<Size> {
    let winsize = rustix::termios::tcgetwinsize(self.stdin_fd)?;
    Ok(Size {
      height: winsize.ws_row,
      width: winsize.ws_col,
    })
  }

  #[cfg(windows)]
  pub fn size(&self) -> std::io::Result<Size> {
    unsafe {
      use std::os::windows::io::AsRawHandle;

      use ::windows::Win32::{
        Foundation::HANDLE,
        System::Console::{
          CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo,
        },
      };

      let mut csbi: CONSOLE_SCREEN_BUFFER_INFO = Default::default();

      GetConsoleScreenBufferInfo(
        HANDLE(self.stdout.as_raw_handle()),
        &mut csbi,
      )?;

      Ok(Size {
        height: (csbi.srWindow.Bottom - csbi.srWindow.Top + 1) as u16,
        width: (csbi.srWindow.Right - csbi.srWindow.Left + 1) as u16,
      })
    }
  }
}

impl Drop for TermDriver {
  fn drop(&mut self) {
    let mut seq: Vec<u8> = Vec::new();
    match self.keyboard {
      KeyboardMode::Unknown => (),
      KeyboardMode::ModifyOtherKeys => emit::modify_other_keys(&mut seq, 0),
      KeyboardMode::Kitty => seq.extend_from_slice(emit::KITTY_POP.as_bytes()),
      KeyboardMode::Win32 => {
        emit::dec_reset(&mut seq, emit::DecMode::Win32Input)
      }
    }

    emit::dec_reset(&mut seq, emit::DecMode::BracketedPaste);
    emit::dec_reset(&mut seq, emit::DecMode::MouseSgr);
    emit::dec_reset(&mut seq, emit::DecMode::MouseRxvt);
    emit::dec_reset(&mut seq, emit::DecMode::MouseAnyMotion);
    emit::dec_reset(&mut seq, emit::DecMode::MouseButtonMotion);
    emit::dec_reset(&mut seq, emit::DecMode::MousePressRelease);
    emit::dec_reset(&mut seq, emit::DecMode::AltScreen);

    // Save/Restore does not work on tmux. So we just show cursor.
    emit::dec_set(&mut seq, emit::DecMode::ShowCursor);
    seq.extend_from_slice(emit::RESTORE_CURSOR.as_bytes());

    self.stdout.write_all(&seq).log_ignore();
    self.stdout.flush().log_ignore();

    #[cfg(unix)]
    rustix::io::write(&self.stdin_wakeup, &[0]).log_ignore();

    #[cfg(unix)]
    if let Some(stdin_thread) = self.stdin_thread.take() {
      stdin_thread.join().ok();
    }

    #[cfg(unix)]
    rustix::termios::tcsetattr(
      self.stdin_fd,
      rustix::termios::OptionalActions::Now,
      &self.orig_termios,
    )
    .log_ignore();

    #[cfg(windows)]
    self.win_vt.disable();
  }
}
