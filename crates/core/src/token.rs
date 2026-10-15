//! A token an account on a service is reached with.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A token an account on a service is reached with.
///
/// `{:?}` prints no part of it, so a token formatted into a message, a log
/// line or the output of a failed test is not given away there. The text is
/// read with [`Token::secret`] and serialised whole, where the token is sent
/// or kept.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    /// A token of this text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text of the token.
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Token(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::Token;

    #[test]
    fn a_token_formatted_for_debugging_shows_none_of_its_text() {
        let token = Token::new("header.payload.signature");

        assert_eq!(format!("{token:?}"), "Token(..)");
        assert_eq!(format!("{:?}", Some(&token)), "Some(Token(..))");
    }

    #[test]
    fn a_token_travels_as_its_text() {
        let token = Token::new("header.payload.signature");

        let text = serde_json::to_string(&token).expect("serialise");
        let back: Token = serde_json::from_str(&text).expect("deserialise");

        assert_eq!(text, "\"header.payload.signature\"");
        assert_eq!(back.secret(), "header.payload.signature");
    }
}
