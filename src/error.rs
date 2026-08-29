pub trait ResultLogger<R> {
  fn log_ignore(&self) -> ();
}

impl<R, E: ToString> ResultLogger<R> for Result<R, E> {
  fn log_ignore(&self) {
    match self {
      Ok(_) => (),
      Err(err) => log::debug!("Error: {}", err.to_string()),
    }
  }
}
