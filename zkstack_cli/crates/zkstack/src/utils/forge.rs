use anyhow::Context as _;
use common::{forge::ForgeScript, wallets::Wallet};
use ethers::types::U256;

use crate::{
    consts::MINIMUM_BALANCE_FOR_WALLET,
    messages::{msg_address_doesnt_have_enough_money_prompt, msg_wallet_private_key_not_set},
};

pub enum WalletType {
    Governor,
    Deployer,
}

pub fn fill_forge_private_key(
    mut forge: ForgeScript,
    wallet: Option<&Wallet>,
    wallet_type: WalletType,
) -> anyhow::Result<ForgeScript> {
    if !forge.wallet_args_passed() {
        forge = forge.with_private_key(
            wallet
                .and_then(|w| w.private_key_h256())
                .context(msg_wallet_private_key_not_set(wallet_type))?,
        );
    }
    Ok(forge)
}

pub async fn check_the_balance(forge: &ForgeScript) -> anyhow::Result<()> {
    let Some(address) = forge.address() else {
        return Ok(());
    };

    let expected_balance = U256::from(MINIMUM_BALANCE_FOR_WALLET);
    while let Some(balance) = forge.get_the_balance().await? {
        if balance >= expected_balance {
            return Ok(());
        }
        if !common::PromptConfirm::new(msg_address_doesnt_have_enough_money_prompt(
            &address,
            balance,
            expected_balance,
        ))
        .ask()
        {
            break;
        }
    }
    Ok(())
}
