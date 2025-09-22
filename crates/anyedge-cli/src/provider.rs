use anyedge_adapter_fastly::cli;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Fastly,
}

impl Provider {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "fastly" => Ok(Self::Fastly),
            other => Err(format!("provider `{other}` is not yet supported")),
        }
    }

    pub fn build(&self) -> Result<(), String> {
        match self {
            Provider::Fastly => {
                let artifact = cli::build()?;
                println!("[anyedge] Fastly build complete -> {}", artifact.display());
                Ok(())
            }
        }
    }

    pub fn deploy(&self) -> Result<(), String> {
        match self {
            Provider::Fastly => cli::deploy(),
        }
    }

    pub fn serve(&self) -> Result<(), String> {
        match self {
            Provider::Fastly => cli::serve(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Provider;

    #[test]
    fn parse_fastly() {
        assert!(matches!(Provider::parse("fastly"), Ok(Provider::Fastly)));
        assert!(matches!(Provider::parse("Fastly"), Ok(Provider::Fastly)));
    }

    #[test]
    fn parse_unknown() {
        let err = Provider::parse("unknown").unwrap_err();
        assert!(err.contains("not yet supported"));
    }
}
