use std::{
    borrow::Borrow,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, TryFutureExt};

use crate::{
    config::{NetworkConfig, ServerConfig},
    error::BoxError,
    helpers::{
        query::{PrepareQuery, QueryConfig, QueryInput},
        BodyStream, CompleteQueryResult, HelperIdentity, LogErrors, NoResourceIdentifier,
        PrepareQueryResult, QueryIdBinding, QueryInputResult, QueryStatusResult,
        ReceiveQueryResult, ReceiveRecords, RouteId, RouteParams, StepBinding, StreamCollection,
        Transport, TransportCallbacks,
    },
    net::{client::MpcHelperClient, error::Error, MpcHelperServer},
    protocol::{step::Gate, QueryId},
    sync::Arc,
};

type LogHttpErrors = LogErrors<BodyStream, Bytes, BoxError>;

/// HTTP transport for IPA helper service.
pub struct HttpTransport {
    identity: HelperIdentity,
    callbacks: TransportCallbacks<Arc<HttpTransport>>,
    clients: [MpcHelperClient; 3],
    // TODO(615): supporting multiple queries likely require a hashmap here. It will be ok if we
    // only allow one query at a time.
    record_streams: StreamCollection<LogHttpErrors>,
}

impl HttpTransport {
    #[must_use]
    pub fn new(
        identity: HelperIdentity,
        server_config: ServerConfig,
        network_config: NetworkConfig,
        clients: [MpcHelperClient; 3],
        callbacks: TransportCallbacks<Arc<HttpTransport>>,
    ) -> (Arc<Self>, MpcHelperServer) {
        let transport = Self::new_internal(identity, clients, callbacks);
        let server = MpcHelperServer::new(Arc::clone(&transport), server_config, network_config);
        (transport, server)
    }

    fn new_internal(
        identity: HelperIdentity,
        clients: [MpcHelperClient; 3],
        callbacks: TransportCallbacks<Arc<HttpTransport>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            callbacks,
            clients,
            record_streams: StreamCollection::default(),
        })
    }

    pub fn receive_query(self: Arc<Self>, req: QueryConfig) -> ReceiveQueryResult {
        (Arc::clone(&self).callbacks.receive_query)(self, req)
    }

    pub fn prepare_query(self: Arc<Self>, req: PrepareQuery) -> PrepareQueryResult {
        (Arc::clone(&self).callbacks.prepare_query)(self, req)
    }

    pub fn query_input(self: Arc<Self>, req: QueryInput) -> QueryInputResult {
        (Arc::clone(&self).callbacks.query_input)(self, req)
    }

    pub fn query_status(self: Arc<Self>, query_id: QueryId) -> QueryStatusResult {
        (Arc::clone(&self).callbacks.query_status)(self, query_id)
    }

    pub fn complete_query(self: Arc<Self>, query_id: QueryId) -> CompleteQueryResult {
        /// Cleans up the `records_stream` collection after drop to ensure this transport
        /// can process the next query even in case of a panic.
        struct ClearOnDrop {
            transport: Arc<HttpTransport>,
            qr: CompleteQueryResult,
        }

        impl Future for ClearOnDrop {
            type Output = <CompleteQueryResult as Future>::Output;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                self.qr.as_mut().poll(cx)
            }
        }

        impl Drop for ClearOnDrop {
            fn drop(&mut self) {
                self.transport.record_streams.clear();
            }
        }

        Box::pin(ClearOnDrop {
            transport: Arc::clone(&self),
            qr: Box::pin((Arc::clone(&self).callbacks.complete_query)(self, query_id)),
        })
    }

    /// Connect an inbound stream of MPC record data.
    ///
    /// This is called by peer helpers via the HTTP server.
    pub fn receive_stream(
        self: Arc<Self>,
        query_id: QueryId,
        gate: Gate,
        from: HelperIdentity,
        stream: BodyStream,
    ) {
        self.record_streams
            .add_stream((query_id, from, gate), LogErrors::new(stream));
    }
}

#[async_trait]
impl Transport for Arc<HttpTransport> {
    type RecordsStream = ReceiveRecords<LogHttpErrors>;
    type Error = Error;

    fn identity(&self) -> HelperIdentity {
        self.identity
    }

    async fn send<
        D: Stream<Item = Vec<u8>> + Send + 'static,
        Q: QueryIdBinding,
        S: StepBinding,
        R: RouteParams<RouteId, Q, S>,
    >(
        &self,
        dest: HelperIdentity,
        route: R,
        data: D,
    ) -> Result<(), Error>
    where
        Option<QueryId>: From<Q>,
        Option<Gate>: From<S>,
    {
        let route_id = route.resource_identifier();
        match route_id {
            RouteId::Records => {
                // TODO(600): These fallible extractions aren't really necessary.
                let query_id = <Option<QueryId>>::from(route.query_id())
                    .expect("query_id required when sending records");
                let step =
                    <Option<Gate>>::from(route.gate()).expect("step required when sending records");
                let resp_future = self.clients[dest].step(query_id, &step, data)?;
                // we don't need to spawn a task here. Gateway's sender interface already does that
                // so this can just poll this future.
                resp_future
                    .map_err(Into::into)
                    .and_then(MpcHelperClient::resp_ok)
                    .await?;
                Ok(())
            }
            RouteId::PrepareQuery => {
                let req = serde_json::from_str(route.extra().borrow()).unwrap();
                self.clients[dest].prepare_query(req).await
            }
            RouteId::ReceiveQuery => {
                unimplemented!("attempting to send ReceiveQuery to another helper")
            }
        }
    }

    fn receive<R: RouteParams<NoResourceIdentifier, QueryId, Gate>>(
        &self,
        from: HelperIdentity,
        route: R,
    ) -> Self::RecordsStream {
        ReceiveRecords::new(
            (route.query_id(), from, route.gate()),
            self.record_streams.clone(),
        )
    }
}

#[cfg(all(test, web_test))]
mod tests {
    use std::{iter::zip, net::TcpListener, task::Poll};

    use futures::stream::{poll_immediate, StreamExt};
    use futures_util::future::{join_all, try_join_all};
    use generic_array::GenericArray;
    use once_cell::sync::Lazy;
    use tokio::sync::mpsc::channel;
    use tokio_stream::wrappers::ReceiverStream;
    use typenum::Unsigned;

    use super::*;
    use crate::{
        config::{NetworkConfig, ServerConfig},
        ff::{FieldType, Fp31, Serializable},
        helpers::query::QueryType::TestMultiply,
        net::{
            client::ClientIdentity,
            test::{get_test_identity, TestConfig, TestConfigBuilder, TestServer},
        },
        secret_sharing::{replicated::semi_honest::AdditiveShare, IntoShares},
        test_fixture::Reconstruct,
        AppSetup, HelperApp,
    };

    static STEP: Lazy<Gate> = Lazy::new(|| Gate::from("http-transport"));

    #[tokio::test]
    async fn receive_stream() {
        let (tx, rx) = channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(1);
        let expected_chunk1 = vec![0u8, 1, 2, 3];
        let expected_chunk2 = vec![255u8, 254, 253, 252];

        let TestServer { transport, .. } = TestServer::default().await;

        let body = BodyStream::from_body(
            Box::new(ReceiverStream::new(rx)) as Box<dyn Stream<Item = _> + Send>
        );

        // Register the stream with the transport (normally called by step data HTTP API handler)
        Arc::clone(&transport).receive_stream(QueryId, STEP.clone(), HelperIdentity::TWO, body);

        // Request step data reception (normally called by protocol)
        let mut stream =
            Arc::clone(&transport).receive(HelperIdentity::TWO, (QueryId, STEP.clone()));

        // make sure it is not ready as it hasn't received any data yet.
        assert!(matches!(
            poll_immediate(&mut stream).next().await,
            Some(Poll::Pending)
        ));

        // send and verify first chunk
        tx.send(Ok(expected_chunk1.clone().into())).await.unwrap();

        assert_eq!(
            poll_immediate(&mut stream).next().await,
            Some(Poll::Ready(expected_chunk1))
        );

        // send and verify second chunk
        tx.send(Ok(expected_chunk2.clone().into())).await.unwrap();

        assert_eq!(
            poll_immediate(&mut stream).next().await,
            Some(Poll::Ready(expected_chunk2))
        );
    }

    // TODO(651): write a test for an error while reading the body (after error handling is finalized)

    async fn make_helpers(
        sockets: [TcpListener; 3],
        server_config: [ServerConfig; 3],
        network_config: &NetworkConfig,
        disable_https: bool,
    ) -> [HelperApp; 3] {
        join_all(
            zip(HelperIdentity::make_three(), zip(sockets, server_config)).map(
                |(id, (socket, server_config))| async move {
                    let identity = if disable_https {
                        ClientIdentity::Helper(id)
                    } else {
                        get_test_identity(id)
                    };
                    let (setup, callbacks) = AppSetup::new();
                    let clients = MpcHelperClient::from_conf(network_config, identity);
                    let (transport, server) = HttpTransport::new(
                        id,
                        server_config,
                        network_config.clone(),
                        clients,
                        callbacks,
                    );
                    server.start_on(Some(socket), ()).await;
                    let app = setup.connect(transport);
                    app
                },
            ),
        )
        .await
        .try_into()
        .ok()
        .unwrap()
    }

    async fn test_three_helpers(mut conf: TestConfig) {
        let clients = MpcHelperClient::from_conf(&conf.network, ClientIdentity::None);
        let _helpers = make_helpers(
            conf.sockets.take().unwrap(),
            conf.servers,
            &conf.network,
            conf.disable_https,
        )
        .await;

        test_multiply(&clients).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn happy_case_twice() {
        let mut conf = TestConfigBuilder::with_open_ports().build();
        let clients = MpcHelperClient::from_conf(&conf.network, ClientIdentity::None);
        let _helpers = make_helpers(
            conf.sockets.take().unwrap(),
            conf.servers,
            &conf.network,
            conf.disable_https,
        )
        .await;

        test_multiply(&clients).await;
        test_multiply(&clients).await;
    }

    async fn test_multiply(clients: &[MpcHelperClient; 3]) {
        const SZ: usize = <AdditiveShare<Fp31> as Serializable>::Size::USIZE;

        // send a create query command
        let leader_client = &clients[0];
        let create_data = QueryConfig::new(TestMultiply, FieldType::Fp31, 1).unwrap();

        // create query
        let query_id = leader_client.create_query(create_data).await.unwrap();

        // send input
        let a = Fp31::try_from(4u128).unwrap();
        let b = Fp31::try_from(5u128).unwrap();

        let helper_shares = (a, b).share().map(|(a, b)| {
            let mut vec = vec![0u8; 2 * SZ];
            a.serialize(GenericArray::from_mut_slice(&mut vec[..SZ]));
            b.serialize(GenericArray::from_mut_slice(&mut vec[SZ..]));
            BodyStream::from(vec)
        });

        let mut handle_resps = Vec::with_capacity(helper_shares.len());
        for (i, input_stream) in helper_shares.into_iter().enumerate() {
            let data = QueryInput {
                query_id,
                input_stream,
            };
            handle_resps.push(clients[i].query_input(data));
        }
        try_join_all(handle_resps).await.unwrap();

        let result: [_; 3] = join_all(clients.clone().map(|client| async move {
            let r = client.query_results(query_id).await.unwrap();
            AdditiveShare::<Fp31>::from_byte_slice(&r).collect::<Vec<_>>()
        }))
        .await
        .try_into()
        .unwrap();
        let res = result.reconstruct();
        assert_eq!(Fp31::try_from(20u128).unwrap(), res[0]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_helpers_http() {
        let conf = TestConfigBuilder::with_open_ports()
            .with_disable_https_option(true)
            .build();
        test_three_helpers(conf).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_helpers_https() {
        let conf = TestConfigBuilder::with_open_ports().build();
        test_three_helpers(conf).await;
    }
}
