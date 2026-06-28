# fundskit

SEC Form 13F institutional holdings for Rust.

```toml
[dependencies]
fundskit = "0.1.0"
```

```rust,no_run
#[tokio::main]
async fn main() -> fundskit::Result<()> {
    // Managers holding Apple (CUSIP 037833100), largest position first.
    for h in fundskit::holders_of("037833100").await?.iter().take(5) {
        println!("{} {} shares ${}", h.manager_name, h.shares, h.value_usd);
    }
    Ok(())
}
```

Positions are identified by CUSIP and issuer name; the `ticker` column is present but empty (there is no free, clean CUSIP-to-ticker map).

Full documentation: <https://github.com/userFRM/fundskit>

Licensed under MIT OR Apache-2.0.
