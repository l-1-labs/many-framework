pub mod common;
use common::*;
use many::server::module::account::features::multisig::*;
use many::{
    server::module::account::{self, AccountModuleBackend},
    server::module::{self, account::features::multisig::AccountMultisigModuleBackend},
    types::{self, identity::testing::identity},
    Identity,
};
use many_ledger::module::LedgerModuleImpl;
use proptest::prelude::*;
use proptest::test_runner::Config;
use std::str::FromStr;

/// Returns informations about the given account
fn account_info(
    module_impl: &mut LedgerModuleImpl,
    id: &Identity,
    account_id: Identity,
) -> account::InfoReturn {
    AccountModuleBackend::info(
        module_impl,
        id,
        account::InfoArgs {
            account: account_id,
        },
    )
    .unwrap()
}

/// Returns the multisig account feature arguments
fn account_arguments(
    module_impl: &mut LedgerModuleImpl,
    id: &Identity,
    account_id: Identity,
) -> account::features::multisig::MultisigAccountFeatureArg {
    account_info(module_impl, id, account_id)
        .features
        .get::<account::features::multisig::MultisigAccountFeature>()
        .unwrap()
        .arg
}

/// Generate some SubmitTransactionArgs for testing
fn submit_args(
    account_id: Identity,
    transaction: types::ledger::AccountMultisigTransaction,
    execute_automatically: Option<bool>,
) -> account::features::multisig::SubmitTransactionArgs {
    account::features::multisig::SubmitTransactionArgs {
        account: account_id,
        memo: Some("Foo".to_string()),
        transaction: Box::new(transaction),
        threshold: None,
        timeout_in_secs: None,
        execute_automatically,
        data: None,
    }
}

/// Returns the multisig transaction info
fn tx_info(
    module_impl: &mut LedgerModuleImpl,
    id: Identity,
    token: &minicbor::bytes::ByteVec,
) -> module::account::features::multisig::InfoReturn {
    let result = module_impl.multisig_info(
        &id,
        account::features::multisig::InfoArgs {
            token: token.clone(),
        },
    );
    assert!(result.is_ok());
    result.unwrap()
}

/// Return the transaction approbation status for the given identity
fn get_approbation(info: &InfoReturn, id: &Identity) -> bool {
    if let Some(value) = info.approvers.get(id) {
        value.approved
    } else {
        panic!("Can't verify approbation; ID not found")
    }
}

#[test]
/// Verify owner can submit a transaction
fn submit_transaction() {
    let SetupWithAccountAndTx {
        mut module_impl,
        id,
        account_id,
        tx,
    } = setup_with_account_and_tx();

    let submit_args = submit_args(account_id, tx.clone(), None);
    let result = module_impl.multisig_submit_transaction(&id, submit_args.clone());
    assert!(result.is_ok());

    let tx_info = tx_info(&mut module_impl, id, &result.unwrap().token);
    assert_eq!(tx_info.memo, submit_args.memo);
    assert_eq!(tx_info.transaction, tx);
    assert_eq!(tx_info.submitter, id);
    assert!(get_approbation(&tx_info, &id));
    assert_eq!(tx_info.threshold, 3);
    assert!(!tx_info.execute_automatically);
    assert_eq!(tx_info.data, submit_args.data);
}

#[test]
/// Verify identity with `canMultisigSubmit` can submit a transaction
fn submit_transaction_valid_role() {
    let SetupWithAccountAndTx {
        mut module_impl,
        account_id,
        tx,
        ..
    } = setup_with_account_and_tx();
    let result =
        module_impl.multisig_submit_transaction(&identity(3), submit_args(account_id, tx, None));
    assert!(result.is_ok());
}

#[test]
/// Verify identity with `canMultisigApprove` can't submit a transaction
fn submit_transaction_invalid_role() {
    let SetupWithAccountAndTx {
        mut module_impl,
        account_id,
        tx,
        ..
    } = setup_with_account_and_tx();
    let result =
        module_impl.multisig_submit_transaction(&identity(2), submit_args(account_id, tx, None));
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().code,
        account::errors::user_needs_role("").code
    );
}

#[test]
/// Veryfy owner can set new defaults
fn set_defaults() {
    let SetupWithAccount {
        mut module_impl,
        id,
        account_id,
    } = setup_with_account();
    let result = module_impl.multisig_set_defaults(
        &id,
        account::features::multisig::SetDefaultsArgs {
            account: account_id,
            threshold: Some(1),
            timeout_in_secs: Some(12),
            execute_automatically: Some(true),
        },
    );
    assert!(result.is_ok());

    let arguments = account_arguments(&mut module_impl, &id, account_id);
    assert_eq!(arguments.threshold, Some(1));
    assert_eq!(arguments.timeout_in_secs, Some(12));
    assert_eq!(arguments.execute_automatically, Some(true));
}

proptest! {
    #[test]
    /// Verify non-owner are unable to change the defaults
    fn set_defaults_invalid_user(seed in 4..u32::MAX) {
        let SetupWithAccount {
            mut module_impl,
            id,
            account_id,
        } = setup_with_account();
        let result = module_impl.multisig_set_defaults(
            &identity(seed),
            account::features::multisig::SetDefaultsArgs {
                account: account_id,
                threshold: Some(1),
                timeout_in_secs: Some(12),
                execute_automatically: Some(true),
            },
        );
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            account::errors::user_needs_role("").code
        );

        let arguments = account_arguments(&mut module_impl, &id, account_id);
        assert_eq!(arguments.threshold, Some(3));
        assert_eq!(
            arguments.timeout_in_secs,
            Some(many_ledger::storage::MULTISIG_DEFAULT_TIMEOUT_IN_SECS)
        );
        assert_eq!(
            arguments.execute_automatically,
            Some(many_ledger::storage::MULTISIG_DEFAULT_EXECUTE_AUTOMATICALLY)
        );
    }
}

#[test]
/// Verify identity with `canMultisigApprove` and identity with `canMultisigSubmit` can approve a transaction
fn approve() {
    let SetupWithAccountAndTx {
        mut module_impl,
        id,
        account_id,
        tx,
    } = setup_with_account_and_tx();
    let result = module_impl.multisig_submit_transaction(&id, submit_args(account_id, tx, None));
    assert!(result.is_ok());
    let submit_return = result.unwrap();
    let info = tx_info(&mut module_impl, id, &submit_return.token);
    assert!(get_approbation(&info, &id));
    assert_eq!(info.threshold, 3);

    let result = module_impl.multisig_approve(
        &identity(2),
        account::features::multisig::ApproveArgs {
            token: submit_return.clone().token,
        },
    );
    assert!(result.is_ok());
    assert!(get_approbation(
        &tx_info(&mut module_impl, id, &submit_return.token),
        &identity(2)
    ));

    let result = module_impl.multisig_approve(
        &identity(3),
        account::features::multisig::ApproveArgs {
            token: submit_return.clone().token,
        },
    );
    assert!(result.is_ok());
    assert!(get_approbation(
        &tx_info(&mut module_impl, id, &submit_return.token),
        &identity(3)
    ));
}

#[test]
/// Verify identity not part of the account can't approve a transaction
fn approve_invalid() {
    let SetupWithAccountAndTx {
        mut module_impl,
        id,
        account_id,
        tx,
    } = setup_with_account_and_tx();
    let result = module_impl.multisig_submit_transaction(&id, submit_args(account_id, tx, None));
    assert!(result.is_ok());
    let submit_return = result.unwrap();
    let info = tx_info(&mut module_impl, id, &submit_return.token);
    assert!(get_approbation(&info, &id));
    assert_eq!(info.threshold, 3);

    let result = module_impl.multisig_approve(
        &identity(6),
        account::features::multisig::ApproveArgs {
            token: submit_return.clone().token,
        },
    );
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().code,
        account::features::multisig::errors::user_cannot_approve_transaction().code
    );
}

#[test]
/// Verify identity with `owner`, `canMultisigSubmit` and `canMultisigApprove` can revoke a transaction
fn revoke() {
    let SetupWithAccountAndTx {
        mut module_impl,
        id,
        account_id,
        tx,
    } = setup_with_account_and_tx();
    let result = module_impl.multisig_submit_transaction(&id, submit_args(account_id, tx, None));
    assert!(result.is_ok());
    let token = result.unwrap().token;
    let info = tx_info(&mut module_impl, id, &token);
    assert!(get_approbation(&info, &id));
    assert_eq!(info.threshold, 3);

    for i in [id, identity(2), identity(3)] {
        let result = module_impl.multisig_approve(
            &i,
            account::features::multisig::ApproveArgs {
                token: token.clone(),
            },
        );
        assert!(result.is_ok());
        assert!(get_approbation(&tx_info(&mut module_impl, i, &token), &i));

        let result = module_impl.multisig_revoke(
            &i,
            account::features::multisig::RevokeArgs {
                token: token.clone(),
            },
        );
        assert!(result.is_ok());
        assert!(!get_approbation(&tx_info(&mut module_impl, i, &token), &i));
    }
}

#[test]
/// Verify identity not part of the account can't revoke a transaction
fn revoke_invalid() {
    let SetupWithAccountAndTx {
        mut module_impl,
        id,
        account_id,
        tx,
    } = setup_with_account_and_tx();
    let result = module_impl.multisig_submit_transaction(&id, submit_args(account_id, tx, None));
    assert!(result.is_ok());
    let token = result.unwrap().token;
    assert!(get_approbation(&tx_info(&mut module_impl, id, &token), &id));

    let result = module_impl.multisig_revoke(
        &identity(6),
        account::features::multisig::RevokeArgs { token },
    );
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().code,
        account::features::multisig::errors::user_cannot_approve_transaction().code
    );
}

proptest! {
    #![proptest_config(Config { cases: 2, source_file: Some("tests/multisig"), .. Config::default() })]
    #[test]
    /// Verify we can execute a transaction when the threshold is reached
    /// Both manual and automatic execution are tested
    fn execute(execute_automatically in any::<bool>()) {
        let SetupWithAccountAndTx {
            mut module_impl,
            id,
            account_id,
            tx,
        } = setup_with_account_and_tx();
        module_impl.set_balance_only_for_testing(
            account_id,
            10000,
            Identity::from_str("mqbfbahksdwaqeenayy2gxke32hgb7aq4ao4wt745lsfs6wiaaaaqnz").unwrap(),
        );
        let result = module_impl.multisig_submit_transaction(&id, submit_args(account_id, tx, Some(execute_automatically)));
        assert!(result.is_ok());
        let token = result.unwrap().token;
        let info = tx_info(&mut module_impl, id, &token);
        assert!(get_approbation(&info, &id));
        assert_eq!(info.threshold, 3);

        let identities = [id, identity(2), identity(3)];
        let last = identities.last().unwrap();
        for i in identities.into_iter() {
            // Approve with the current identity
            let result = module_impl.multisig_approve(
                &i,
                account::features::multisig::ApproveArgs {
                    token: token.clone(),
                },
            );
            assert!(result.is_ok());

            // Try to execute the transaction. It should error for every
            // identity since the last identity is NOT an owner nor the
            // submitter of the transaction
            let result = module_impl.multisig_execute(
                &i,
                account::features::multisig::ExecuteArgs {
                    token: token.clone(),
                },
            );
            assert!(result.is_err());

            if &i == last {
                // At this point, everyone has approved. We can execute the
                // transaction using the owner/submitter identity.
                let result = module_impl.multisig_execute(
                    &id,
                    account::features::multisig::ExecuteArgs {
                        token: token.clone(),
                    },
                );
                if execute_automatically {
                    // Transaction was automatically executed, trying to execute
                    // it manually returns an error.
                    assert!(result.is_err());
                    assert_eq!(
                        result.unwrap_err().code,
                        account::features::multisig::errors::transaction_cannot_be_found().code
                    );
                } else {
                    // We have enough approvers and the manual execution succeeded.
                    assert!(result.is_ok());
                    assert!(result.unwrap().data.is_ok());
                }
            } else {
                // Not enough approbation for execution yet.
                assert!(result.is_err());
                assert_eq!(
                    result.unwrap_err().code,
                    account::features::multisig::errors::cannot_execute_transaction().code
                );
                assert!(get_approbation(&tx_info(&mut module_impl, i, &token), &i));
            }
        }
    }
}

#[test]
/// Verify identities with `owner` and `canMultisigSubmit` can withdraw a transaction
fn withdraw() {
    let SetupWithAccountAndTx {
        mut module_impl,
        id,
        account_id,
        tx,
    } = setup_with_account_and_tx();
    for i in [id, identity(3)] {
        let result =
            module_impl.multisig_submit_transaction(&i, submit_args(account_id, tx.clone(), None));
        assert!(result.is_ok());
        let token = result.unwrap().token;

        let result = module_impl.multisig_withdraw(
            &i,
            account::features::multisig::WithdrawArgs {
                token: token.clone(),
            },
        );
        assert!(result.is_ok());
        let result = module_impl.multisig_info(&i, account::features::multisig::InfoArgs { token });
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            account::features::multisig::errors::transaction_cannot_be_found().code
        );
    }
}

#[test]
/// Verify identity with `canMultisigApprove` and identity not part of the account can't withdraw a transaction
fn withdraw_invalid() {
    let SetupWithAccountAndTx {
        mut module_impl,
        id,
        account_id,
        tx,
    } = setup_with_account_and_tx();
    let result = module_impl.multisig_submit_transaction(&id, submit_args(account_id, tx, None));
    assert!(result.is_ok());
    let token = result.unwrap().token;
    for i in [identity(2), identity(6)] {
        let result = module_impl.multisig_withdraw(
            &i,
            account::features::multisig::WithdrawArgs {
                token: token.clone(),
            },
        );
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            account::features::multisig::errors::cannot_execute_transaction().code
        );
    }
}
