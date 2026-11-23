//! Error chains appear in logs, so every cause must be visible.

use std::{error::Error, fmt};

use mojang_api::error::ErrorChain;

#[derive(Debug)]
struct Layer {
    message: &'static str,
    source: Option<Box<Self>>,
}

impl fmt::Display for Layer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for Layer {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[test]
fn shows_every_cause_of_an_error() {
    let error = Layer {
        message: "request failed",
        source: Some(Box::new(Layer {
            message: "connection reset",
            source: Some(Box::new(Layer {
                message: "broken pipe",
                source: None,
            })),
        })),
    };

    assert_eq!(
        ErrorChain(&error).to_string(),
        "request failed: connection reset: broken pipe"
    );
    assert_eq!(
        ErrorChain(&Layer {
            message: "alone",
            source: None,
        })
        .to_string(),
        "alone"
    );
}
