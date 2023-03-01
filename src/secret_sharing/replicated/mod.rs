use super::{SecretSharing, SharedValue};

pub mod malicious;
pub mod semi_honest;

pub trait ReplicatedSecretSharing<V: SharedValue>: SecretSharing<V> {
    fn left(&self) -> V;
    fn right(&self) -> V;
}
