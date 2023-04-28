use std::{
    fmt::{Debug, Formatter},
    num::NonZeroUsize,
};

mod buffers;
mod error;
mod gateway;
mod prss_protocol;
mod transport;

pub use error::{Error, Result};
pub use gateway::{GatewayConfig, ReceivingEnd, SendingEnd};

// TODO: this type should only be available within infra. Right now several infra modules
// are exposed at the root level. That makes it impossible to have a proper hierarchy here.
pub use gateway::{Gateway, TransportError, TransportImpl};

pub use prss_protocol::negotiate as negotiate_prss;
pub use transport::{
    AlignedByteArrStream, ByteArrStream, NoResourceIdentifier, PrepareQueryCallback,
    QueryIdBinding, ReceiveQueryCallback, RouteId, RouteParams, StepBinding, Transport,
    TransportCallbacks,
};

pub use transport::query;

/// to validate that transport can actually send streams of this type
#[cfg(test)]
pub use buffers::OrderingSender;

use crate::{
    ff::Serializable,
    helpers::{
        Direction::{Left, Right},
        Role::{H1, H2, H3},
    },
    protocol::{GenericStep, RecordId},
    secret_sharing::SharedValue,
};
use generic_array::GenericArray;
use std::ops::{Index, IndexMut};
use typenum::{Unsigned, U8};
use x25519_dalek::PublicKey;

// TODO work with ArrayLength only
pub type MessagePayloadArrayLen = U8;

pub const MESSAGE_PAYLOAD_SIZE_BYTES: usize = MessagePayloadArrayLen::USIZE;

/// Represents an opaque identifier of the helper instance. Compare with a [`Role`], which
/// represents a helper's role within an MPC protocol, which may be different per protocol.
/// `HelperIdentity` will be established at startup and then never change. Components that want to
/// resolve this identifier into something (Uri, encryption keys, etc) must consult configuration
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
#[cfg_attr(
    feature = "enable-serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(transparent)
)]
pub struct HelperIdentity {
    id: u8,
}

impl TryFrom<usize> for HelperIdentity {
    type Error = String;

    fn try_from(value: usize) -> std::result::Result<Self, Self::Error> {
        if value == 0 || value > 3 {
            Err(format!(
                "{value} must be within [1, 3] range to be a valid helper identity"
            ))
        } else {
            Ok(Self {
                id: u8::try_from(value).unwrap(),
            })
        }
    }
}

impl Debug for HelperIdentity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self.id {
                1 => "A",
                2 => "B",
                3 => "C",
                _ => unreachable!(),
            }
        )
    }
}

#[cfg(feature = "web-app")]
impl From<HelperIdentity> for hyper::header::HeaderValue {
    fn from(id: HelperIdentity) -> Self {
        // does not implement `From<u8>`
        hyper::header::HeaderValue::from(u16::from(id.id))
    }
}

#[cfg(test)]
impl From<i32> for HelperIdentity {
    fn from(value: i32) -> Self {
        usize::try_from(value)
            .ok()
            .and_then(|id| HelperIdentity::try_from(id).ok())
            .unwrap()
    }
}

impl HelperIdentity {
    pub const ONE: Self = Self { id: 1 };
    pub const TWO: Self = Self { id: 2 };
    pub const THREE: Self = Self { id: 3 };

    /// Given a helper identity, return an array of the identities of the other two helpers.
    // The order that helpers are returned here is not intended to be meaningful, however,
    // it is currently used directly to determine the assignment of roles in
    // `Processor::new_query`.
    #[must_use]
    pub fn others(&self) -> [HelperIdentity; 2] {
        match self.id {
            1 => [Self::TWO, Self::THREE],
            2 => [Self::THREE, Self::ONE],
            3 => [Self::ONE, Self::TWO],
            _ => unreachable!("helper identity out of range"),
        }
    }
}

#[cfg(any(test, feature = "test-fixture"))]
impl HelperIdentity {
    #[must_use]
    #[allow(clippy::missing_panics_doc)]
    pub fn make_three() -> [Self; 3] {
        [
            Self::try_from(1).unwrap(),
            Self::try_from(2).unwrap(),
            Self::try_from(3).unwrap(),
        ]
    }
}

// `HelperIdentity` is 1-indexed, so subtract 1 for `Index` values
impl<T> Index<HelperIdentity> for [T] {
    type Output = T;

    fn index(&self, index: HelperIdentity) -> &Self::Output {
        self.index(usize::from(index.id) - 1)
    }
}

// `HelperIdentity` is 1-indexed, so subtract 1 for `Index` values
impl<T> IndexMut<HelperIdentity> for [T] {
    fn index_mut(&mut self, index: HelperIdentity) -> &mut Self::Output {
        self.index_mut(usize::from(index.id) - 1)
    }
}

impl<T> Index<HelperIdentity> for Vec<T> {
    type Output = T;

    fn index(&self, index: HelperIdentity) -> &Self::Output {
        self.as_slice().index(index)
    }
}

impl<T> IndexMut<HelperIdentity> for Vec<T> {
    fn index_mut(&mut self, index: HelperIdentity) -> &mut Self::Output {
        self.as_mut_slice().index_mut(index)
    }
}

/// Represents a unique role of the helper inside the MPC circuit. Each helper may have different
/// roles in queries it processes in parallel. For some queries it can be `H1` and for others it
/// may be `H2` or `H3`.
/// Each helper instance must be able to take any role, but once the role is assigned, it cannot
/// be changed for the remainder of the query.
#[derive(Copy, Clone, Debug, PartialEq, Hash, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[cfg_attr(
    feature = "enable-serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(into = "&'static str", try_from = "&str")
)]
pub enum Role {
    H1 = 0,
    H2 = 1,
    H3 = 2,
}

#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[cfg_attr(
    feature = "enable-serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(transparent)
)]
pub struct RoleAssignment {
    helper_roles: [HelperIdentity; 3],
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
}

impl Role {
    const H1_STR: &'static str = "H1";
    const H2_STR: &'static str = "H2";
    const H3_STR: &'static str = "H3";

    #[must_use]
    pub fn all() -> &'static [Role; 3] {
        const VARIANTS: &[Role; 3] = &[Role::H1, Role::H2, Role::H3];

        VARIANTS
    }

    /// Returns the role of a peer that is located at the specified direction
    #[must_use]
    pub fn peer(&self, direction: Direction) -> Role {
        match (self, direction) {
            (H1, Left) | (H2, Right) => H3,
            (H1, Right) | (H3, Left) => H2,
            (H3, Right) | (H2, Left) => H1,
        }
    }

    #[must_use]
    pub fn as_static_str(&self) -> &'static str {
        match self {
            H1 => Role::H1_STR,
            H2 => Role::H2_STR,
            H3 => Role::H3_STR,
        }
    }
}

impl From<Role> for &'static str {
    fn from(role: Role) -> Self {
        role.as_static_str()
    }
}

impl TryFrom<&str> for Role {
    type Error = crate::error::Error;

    fn try_from(id: &str) -> std::result::Result<Self, Self::Error> {
        match id {
            Role::H1_STR => Ok(H1),
            Role::H2_STR => Ok(H2),
            Role::H3_STR => Ok(H3),
            other => Err(crate::error::Error::path_parse_error(other)),
        }
    }
}

impl AsRef<str> for Role {
    fn as_ref(&self) -> &str {
        match self {
            H1 => Role::H1_STR,
            H2 => Role::H2_STR,
            H3 => Role::H3_STR,
        }
    }
}

impl<T> Index<Role> for [T] {
    type Output = T;

    fn index(&self, index: Role) -> &Self::Output {
        let idx: usize = match index {
            Role::H1 => 0,
            Role::H2 => 1,
            Role::H3 => 2,
        };

        self.index(idx)
    }
}

impl<T> IndexMut<Role> for [T] {
    fn index_mut(&mut self, index: Role) -> &mut Self::Output {
        let idx: usize = match index {
            Role::H1 => 0,
            Role::H2 => 1,
            Role::H3 => 2,
        };

        self.index_mut(idx)
    }
}

impl<T> Index<Role> for Vec<T> {
    type Output = T;

    fn index(&self, index: Role) -> &Self::Output {
        self.as_slice().index(index)
    }
}

impl<T> IndexMut<Role> for Vec<T> {
    fn index_mut(&mut self, index: Role) -> &mut Self::Output {
        self.as_mut_slice().index_mut(index)
    }
}

impl RoleAssignment {
    #[must_use]
    pub fn new(helper_roles: [HelperIdentity; 3]) -> Self {
        Self { helper_roles }
    }

    /// Returns the assigned role for the given helper identity.
    ///
    /// ## Panics
    /// Panics if there is no role assigned to it.
    #[must_use]
    pub fn role(&self, id: HelperIdentity) -> Role {
        for (idx, item) in self.helper_roles.iter().enumerate() {
            if *item == id {
                return Role::all()[idx];
            }
        }

        panic!("No role assignment for {id:?} found in {self:?}")
    }

    #[must_use]
    pub fn identity(&self, role: Role) -> HelperIdentity {
        self.helper_roles[role]
    }
}

impl TryFrom<[(HelperIdentity, Role); 3]> for RoleAssignment {
    type Error = String;

    fn try_from(value: [(HelperIdentity, Role); 3]) -> std::result::Result<Self, Self::Error> {
        let mut result = [None, None, None];
        for (helper, role) in value {
            if result[role].is_some() {
                return Err(format!("Role {role:?} has been assigned twice"));
            }

            result[role] = Some(helper);
        }

        Ok(RoleAssignment::new(result.map(Option::unwrap)))
    }
}

impl TryFrom<[Role; 3]> for RoleAssignment {
    type Error = String;

    fn try_from(value: [Role; 3]) -> std::result::Result<Self, Self::Error> {
        Self::try_from([
            (HelperIdentity::ONE, value[0]),
            (HelperIdentity::TWO, value[1]),
            (HelperIdentity::THREE, value[2]),
        ])
    }
}

/// Combination of helper role and step that uniquely identifies a single channel of communication
/// between two helpers.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct ChannelId {
    pub role: Role,
    // TODO: step could be either reference or owned value. references are convenient to use inside
    // gateway , owned values can be used inside lookup tables.
    pub step: GenericStep,
}

impl ChannelId {
    #[must_use]
    pub fn new(role: Role, step: GenericStep) -> Self {
        Self { role, step }
    }
}

impl Debug for ChannelId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "channel[{:?},{:?}]", self.role, self.step)
    }
}

/// Trait for messages sent between helpers. Everything needs to be serializable and safe to send.
pub trait Message: Debug + Send + Serializable + 'static + Sized {}

/// Any shared value can be send as a message
impl<V: SharedValue> Message for V {}

impl Serializable for PublicKey {
    type Size = typenum::U32;

    fn serialize(&self, buf: &mut GenericArray<u8, Self::Size>) {
        buf.copy_from_slice(self.as_bytes());
    }

    fn deserialize(buf: &GenericArray<u8, Self::Size>) -> Self {
        Self::from(<[u8; 32]>::from(*buf))
    }
}

impl Message for PublicKey {}

#[derive(Clone, Copy, Debug)]
pub enum TotalRecords {
    Unspecified,
    Specified(NonZeroUsize),

    /// Total number of records is not well-determined. When the record ID is
    /// counting solved_bits attempts. The total record count for solved_bits
    /// depends on the number of failures.
    ///
    /// The purpose of this is to waive the warning that there is a known
    /// number of records when creating a channel.
    ///
    /// Using this is very inefficient, so avoid it.
    Indeterminate,
}

impl TotalRecords {
    #[must_use]
    pub fn is_unspecified(&self) -> bool {
        matches!(self, &TotalRecords::Unspecified)
    }

    #[must_use]
    pub fn is_indeterminate(&self) -> bool {
        matches!(self, &TotalRecords::Indeterminate)
    }

    #[must_use]
    pub fn count(&self) -> Option<usize> {
        match self {
            TotalRecords::Specified(v) => Some(v.get()),
            TotalRecords::Indeterminate | TotalRecords::Unspecified => None,
        }
    }

    /// Returns true iff the total number of records is specified and the given record is the final
    /// one to process.
    #[must_use]
    pub fn is_last<I: Into<RecordId>>(&self, record_id: I) -> bool {
        match self {
            Self::Unspecified | Self::Indeterminate => false,
            Self::Specified(v) => usize::from(record_id.into()) == v.get() - 1,
        }
    }

    /// Overwrite this value.
    /// # Panics
    /// This panics if the transition is invalid.
    /// Any new value is OK if the current value is unspecified.
    /// Otherwise the new value can be indeterminate if the old value is specified.
    #[must_use]
    pub fn overwrite<T: Into<TotalRecords>>(&self, value: T) -> TotalRecords {
        match (self, value.into()) {
            (Self::Unspecified, v) => v,
            (_, Self::Unspecified) => panic!("TotalRecords needs a specific value for overwriting"),
            (Self::Specified(_), Self::Indeterminate) => Self::Indeterminate,
            (old, new) => panic!("TotalRecords bad transition: {old:?} -> {new:?}"),
        }
    }
}

impl From<usize> for TotalRecords {
    fn from(value: usize) -> Self {
        match NonZeroUsize::new(value) {
            Some(v) => TotalRecords::Specified(v),
            None => TotalRecords::Unspecified,
        }
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use super::*;

    mod role_tests {
        use super::*;

        #[test]
        pub fn peer_works() {
            assert_eq!(Role::H1.peer(Direction::Left), Role::H3);
            assert_eq!(Role::H1.peer(Direction::Right), Role::H2);
            assert_eq!(Role::H3.peer(Direction::Left), Role::H2);
            assert_eq!(Role::H3.peer(Direction::Right), Role::H1);
            assert_eq!(Role::H2.peer(Direction::Left), Role::H1);
            assert_eq!(Role::H2.peer(Direction::Right), Role::H3);
        }

        #[test]
        pub fn index_works() {
            let data = [3, 4, 5];
            assert_eq!(3, data[Role::H1]);
            assert_eq!(4, data[Role::H2]);
            assert_eq!(5, data[Role::H3]);
        }
    }

    mod role_assignment_tests {
        use crate::{
            ff::Fp31,
            protocol::{basics::SecureMul, context::Context, RecordId},
            rand::{thread_rng, Rng},
            test_fixture::{Reconstruct, Runner, TestWorld, TestWorldConfig},
        };

        use super::*;

        #[test]
        fn basic() {
            let identities = (1..=3)
                .map(|v| HelperIdentity::try_from(v).unwrap())
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            let assignment = RoleAssignment::new(identities);

            assert_eq!(
                Role::H1,
                assignment.role(HelperIdentity::try_from(1).unwrap())
            );
            assert_eq!(
                Role::H2,
                assignment.role(HelperIdentity::try_from(2).unwrap())
            );
            assert_eq!(
                Role::H3,
                assignment.role(HelperIdentity::try_from(3).unwrap())
            );

            assert_eq!(
                HelperIdentity::try_from(1).unwrap(),
                assignment.identity(Role::H1)
            );
            assert_eq!(
                HelperIdentity::try_from(2).unwrap(),
                assignment.identity(Role::H2)
            );
            assert_eq!(
                HelperIdentity::try_from(3).unwrap(),
                assignment.identity(Role::H3)
            );
        }

        #[test]
        fn reverse() {
            let identities = (1..=3)
                .rev()
                .map(|v| HelperIdentity::try_from(v).unwrap())
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            let assignment = RoleAssignment::new(identities);

            assert_eq!(
                Role::H3,
                assignment.role(HelperIdentity::try_from(1).unwrap())
            );
            assert_eq!(
                Role::H2,
                assignment.role(HelperIdentity::try_from(2).unwrap())
            );
            assert_eq!(
                Role::H1,
                assignment.role(HelperIdentity::try_from(3).unwrap())
            );

            assert_eq!(
                HelperIdentity::try_from(3).unwrap(),
                assignment.identity(Role::H1)
            );
            assert_eq!(
                HelperIdentity::try_from(2).unwrap(),
                assignment.identity(Role::H2)
            );
            assert_eq!(
                HelperIdentity::try_from(1).unwrap(),
                assignment.identity(Role::H3)
            );
        }

        #[test]
        fn illegal() {
            use Role::{H1, H2, H3};

            assert_eq!(
                RoleAssignment::try_from([H1, H1, H3]),
                Err("Role H1 has been assigned twice".into()),
            );

            assert_eq!(
                RoleAssignment::try_from([H3, H2, H3]),
                Err("Role H3 has been assigned twice".into()),
            );
        }

        #[tokio::test]
        async fn multiply_with_various_roles() {
            use Role::{H1, H2, H3};
            const ROLE_PERMUTATIONS: [[Role; 3]; 6] = [
                [H1, H2, H3],
                [H1, H3, H2],
                [H2, H1, H3],
                [H2, H3, H1],
                [H3, H1, H2],
                [H3, H2, H1],
            ];

            for &rp in &ROLE_PERMUTATIONS {
                let config = TestWorldConfig {
                    role_assignment: Some(RoleAssignment::try_from(rp).unwrap()),
                    ..TestWorldConfig::default()
                };

                let world = TestWorld::new_with(config);
                let mut rng = thread_rng();
                let a = rng.gen::<Fp31>();
                let b = rng.gen::<Fp31>();

                let res = world
                    .semi_honest((a, b), |ctx, (a, b)| async move {
                        a.multiply(&b, ctx.set_total_records(1), RecordId::from(0))
                            .await
                            .unwrap()
                    })
                    .await;

                assert_eq!(a * b, res.reconstruct());
            }
        }
    }
}
