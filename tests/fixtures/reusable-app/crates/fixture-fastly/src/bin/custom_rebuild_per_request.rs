fn main() -> Result<(), fastly::Error> {
    fixture_fastly::custom(true, false)
}
