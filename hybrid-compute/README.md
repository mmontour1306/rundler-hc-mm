# AA Hybrid Compute

This repository contains a modified version of the Rundler application which implements a Hybrid Compute capability. Calls to a special `HCHelper` contract are intercepted during the gas-estimation phase, triggering the bundler to make a JSON-RPC call to an external server. The server response is wrapped into a `UserOperation` structure and is front-run ahead of the initiating `UserOperation` in order to populate a response cache in the contract. The gas estimation is then re-run, providing the user with totals reflecting the cost of both their operation and the associated one to populate the cache (implemented by charging extra `preVerificationGas`).

The `deploy-local.py` script in this directory will deploy a Hybrid Compute stack on a local Boba devnet, as produced by running `make devnet-hardhat-up` in a cloned `https://github.com/bobanetwork/boba` repository. The deployer script will fund some L2 accounts and will deploy contracts using Foundry tools. The `docker-compose.yml` file will build two containers, one for the bundler and another for an offchain RPC server containing various example applications.

The `crates/types/contracts/hc_scripts/LocalDeploy.s.sol` script can be adapted for a Testnet deployment of the AA-HC stack. Note that this step is not necessary for developers who wish to register their Offchain appliations with the standard Boba AA-HC implementation. In such cases it is only necessary to set up a HybridAccount contract and to have it registered in the system-wide HCHelper contract. Currently self-registration is not supported so this will require communication with the Boba system maintainers (contact details TBD).

## Deploying a Hybrid Compute application

Each Hybrid Compute application has two pieces - an on-chain Hybrid Account contract and an offchain JSON-RPC service to process the requests. The URL of that service must be associated with the contract's address by calling the `RegisterURL()` method (administrators only) on the `HCHelper` contract. When an authorized contract makes a request to the HybridAccount, the Bundler will pass the request parameters to the JSON-RPC service and will exepct a correctly-formatted response including a signature.

When the HybridAccount is deployed, a nominal amount of testnet ETH must be deposited to the EntryPoint contract. This balance is not spent, but must be present for early validation steps to succeed. This is handled automatically with a deployment script:
```
Generate an OC_OWNER/OC_PRIVKEY address/key pair using whichever method you
prefer. The key will be used by your offchain RPC server to sign its responses,
and does not need to be exposed beyond that server (so a hardware wallet etc.
may be used as long as it is capable of generating the necessary signatures).

You will need a funded deployer account.

$ cd rundler-hc/crates/types/contracts
$ export OC_OWNER=0x1111111111111111111111111111111111111111        # Replace with the address you generated
$ export ENTRY_POINTS=0x2222222222222222222222222222222222222222    # FIXME - Placeholder
$ export HA_FACTORY_ADDR=0x3333333333333333333333333333333333333333 # FIXME - Placeholder

The address of the HybridAccount will be determined by the OC_OWNER address and
by an optional Salt value. If not specified it will default to 0

$ export DEPLOY_SALT=0 # Optional

Run the command, supplying appropriate parameters including the privkey of your
deployer account

$ forge script hc_scripts/DeployHybridAccount.sol --rpc-url=... --private-key=...

If simulation is successful, run the comamand again to send actual transactions
$ forge script hc_scripts/DeployHybridAccount.sol --rpc-url=... --private-key=... --broadcast

The address of the HybridAccount will be returned. Assign this to the
OC_HYBRID_ACCOUNT environment variable.
```

The HybridAccount must also maintain a balance of Boba tokens in the HCHelper contract. These tokens are spent when calls are made. The eventual pricing model is TBD but is currently implemented as a flat fee per call to GetResponse().

Application developers may choose to recover costs from callers to their HybridAccount. No such mechanism is currently provided in accounts created through the default HybridAccountFactory so it would require a custom deployment of a modified contract.

