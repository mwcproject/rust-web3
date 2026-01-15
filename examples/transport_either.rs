use hex_literal::hex;

pub type Transport = mwc_web3::transports::Either<mwc_web3::transports::WebSocket, mwc_web3::transports::Http>;

#[tokio::main]
async fn main() -> mwc_web3::Result {
    let _ = env_logger::try_init();
    let transport = mwc_web3::transports::Http::new("http://localhost:8545")?;

    run(mwc_web3::transports::Either::Right(transport)).await
}

async fn run(transport: Transport) -> mwc_web3::Result {
    let web3 = mwc_web3::Web3::new(transport);

    println!("Calling accounts.");
    let mut accounts = web3.eth().accounts().await?;
    println!("Accounts: {:?}", accounts);
    accounts.push(hex!("00a329c0648769a73afac7f9381e08fb43dbea72").into());

    println!("Calling balance.");
    for account in accounts {
        let balance = web3.eth().balance(account, None).await?;
        println!("Balance of {:?}: {}", account, balance);
    }

    Ok(())
}
