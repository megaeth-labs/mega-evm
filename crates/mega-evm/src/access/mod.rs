//! Volatile-data access tracking (block environment, beneficiary, oracle) behind gas detention.
//!
//! [`VolatileDataAccess`] names the kinds of volatile data a transaction can read. The cap a read
//! sets arrives with gas detention.

mod volatile;

pub use volatile::VolatileDataAccess;
