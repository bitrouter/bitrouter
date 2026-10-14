//! Emit the application configuration schema for contributor tooling.

fn main() -> anyhow::Result<()> {
    print!("{}", bitrouter::config::schema::render()?);
    Ok(())
}
