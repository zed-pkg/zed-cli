//! Polyglot publish fixture (rust slice).

/// Greet, the Rust way. Mirrors the other four slices exactly.
pub fn greet(name: &str) -> String {
    format!("hello, {name}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn greets() {
        assert_eq!(super::greet("zed"), "hello, zed");
    }
}
