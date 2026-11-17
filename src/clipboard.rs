//! Native desktop clipboard, kept alive for X11/Wayland selection ownership.
//! Tests use an in-memory clipboard: headless CI must never access the user's clipboard.

pub struct Clipboard {
    #[cfg(not(test))]
    system: Option<arboard::Clipboard>,
    #[cfg(test)]
    text: Option<String>,
}

impl Clipboard {
    pub fn new() -> Self {
        Self {
            #[cfg(not(test))]
            system: None,
            #[cfg(test)]
            text: None,
        }
    }

    #[cfg(not(test))]
    fn system(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.system.is_none() {
            self.system = Some(arboard::Clipboard::new().map_err(|error| error.to_string())?);
        }
        Ok(self.system.as_mut().expect("clipboard was initialized"))
    }

    pub fn read(&mut self) -> Result<String, String> {
        #[cfg(test)]
        {
            self.text.clone().ok_or_else(|| "Clipboard is empty".into())
        }
        #[cfg(not(test))]
        {
            match self.system()?.get_text() {
                Ok(text) => Ok(text),
                // A cleared clipboard or image is a successful read with no
                // text, not a reason to replay the previous internal register.
                Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
                Err(error) => Err(error.to_string()),
            }
        }
    }

    pub fn write(&mut self, text: &str) -> Result<(), String> {
        #[cfg(test)]
        {
            self.text = Some(text.to_owned());
            Ok(())
        }
        #[cfg(not(test))]
        {
            self.system()?.set_text(text).map_err(|error| error.to_string())
        }
    }
}
