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

#![warn(missing_docs, unreachable_pub)]
#![deny(unused_must_use, rust_2018_idioms)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]

//! Rundler common types

pub mod chain;

/// Generated contracts module
#[allow(non_snake_case)]
#[rustfmt::skip]
#[allow(clippy::all)]
#[allow(missing_docs)]
pub mod contracts;
pub use contracts::shared_types::{DepositInfo, UserOperation, UserOpsPerAggregator};

mod entity;
pub use entity::{Entity, EntityType, EntityUpdate, EntityUpdateType};

mod gas;
pub use gas::GasFees;

mod timestamp;
pub use timestamp::{Timestamp, ValidTimeRange};

mod user_operation;
pub use user_operation::UserOperationId;

mod storage;
pub use storage::StorageSlot;

pub mod hybrid_compute;
