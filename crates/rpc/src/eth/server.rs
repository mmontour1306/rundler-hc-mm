// This file is part of Rundler.
//
// Rundler is free software: you can redistribute it and/or modify it under the
// terms of the GNU Lesser General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later version.
//
// Rundler is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with Rundler.
// If not, see https://www.gnu.org/licenses/.

use async_trait::async_trait;
use ethers::types::{spoof, Address, H256, U64};
use jsonrpsee::core::RpcResult;
use rundler_pool::PoolServer;
use rundler_provider::{EntryPoint, Provider};
use rundler_sim::{GasEstimate, UserOperationOptionalGas};

use super::{api::EthApi, EthApiServer};
use crate::types::{RichUserOperation, RpcUserOperation, UserOperationReceipt};

#[async_trait]
impl<P, E, PS> EthApiServer for EthApi<P, E, PS>
where
    P: Provider,
    E: EntryPoint,
    PS: PoolServer,
{
    async fn send_user_operation(
        &self,
        op: RpcUserOperation,
        entry_point: Address,
    ) -> RpcResult<H256> {
        Ok(EthApi::send_user_operation(self, op, entry_point).await?)
    }

    async fn estimate_user_operation_gas(
        &self,
        op: UserOperationOptionalGas,
        entry_point: Address,
        state_override: Option<spoof::State>,
    ) -> RpcResult<GasEstimate> {
        //println!("HC server.rs est_userOp_gas state {:?}", state_override);

        Ok(EthApi::estimate_user_operation_gas(self, op, entry_point, state_override).await?)
    }

    async fn get_user_operation_by_hash(&self, hash: H256) -> RpcResult<Option<RichUserOperation>> {
        Ok(EthApi::get_user_operation_by_hash(self, hash).await?)
    }

    async fn get_user_operation_receipt(
        &self,
        hash: H256,
    ) -> RpcResult<Option<UserOperationReceipt>> {
        Ok(EthApi::get_user_operation_receipt(self, hash).await?)
    }

    async fn supported_entry_points(&self) -> RpcResult<Vec<String>> {
        Ok(EthApi::supported_entry_points(self).await?)
    }

    async fn chain_id(&self) -> RpcResult<U64> {
        Ok(EthApi::chain_id(self).await?)
    }
}
