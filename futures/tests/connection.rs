// Long and nested future chains can quickly result in large generic types.
#![type_length_limit = "2097152"]

use env_logger;
use failure::Error;
use futures::{Future, Stream};
use lapin_futures as lapin;
use crate::lapin::{BasicProperties, Client, ConnectionProperties};
use crate::lapin::options::{BasicConsumeOptions, BasicPublishOptions, BasicQosOptions, QueueDeclareOptions, QueueDeleteOptions, QueuePurgeOptions};
use crate::lapin::types::FieldTable;
use log::info;
use tokio::runtime::Runtime;

#[test]
fn connection() {
    let _ = env_logger::try_init();

    let addr = std::env::var("AMQP_ADDR").unwrap_or_else(|_| "amqp://127.0.0.1:5672/%2f".into());

    Runtime::new().unwrap().block_on_all(
        Client::connect(&addr, ConnectionProperties::default()).map_err(Error::from).and_then(|client| {
            client.create_channel().and_then(|channel| {
                let id = channel.id();
                info!("created channel with id: {}", id);

                channel.queue_declare("hello", QueueDeclareOptions::default(), FieldTable::default()).and_then(move |_| {
                    info!("channel {} declared queue {}", id, "hello");

                    channel.queue_purge("hello", QueuePurgeOptions::default()).and_then(move |_| {
                        channel.basic_publish("", "hello", b"hello from tokio".to_vec(), BasicPublishOptions::default(), BasicProperties::default())
                    })
                })
            }).and_then(move |_| {
                client.create_channel().map(|ch| (client, ch))
            }).and_then(|(client, channel)| {
                let id = channel.id();
                info!("created channel with id: {}", id);

                let ch1 = channel.clone();
                let ch2 = channel.clone();
                channel.basic_qos(16, BasicQosOptions::default()).and_then(move |_| {
                    info!("channel QoS specified");
                    channel.queue_declare("hello", QueueDeclareOptions::default(), FieldTable::default()).map(move |queue| (channel, queue))
                }).and_then(move |(channel, queue)| {
                    info!("channel {} declared queue {}", id, "hello");

                    channel.basic_consume(&queue, "my_consumer", BasicConsumeOptions::default(), FieldTable::default())
                }).and_then(move |stream| {
                    info!("got consumer stream");

                    stream.into_future().map_err(|(err, _)| err).and_then(move |(message, _)| {
                        let msg = message.unwrap();
                        info!("got message: {:?}", msg);
                        assert_eq!(msg.data, b"hello from tokio");
                        ch1.basic_ack(msg.delivery_tag, false)
                    }).and_then(move |_| {
                        ch2.queue_delete("hello", QueueDeleteOptions::default())
                    })
                        .map(|_| client)
                }).and_then(|client| {
                    client.create_channel()
                        .and_then(|ch| {
                            ch.queue_declare("to_bind", QueueDeclareOptions::default(), FieldTable::default()).map(move |queue| (ch, queue))
                        })
                        .and_then(|(ch, q)| {
                            ch.queue_bind("to_bind", "non_existing_exchange", "my-routing-key", Default::default(), FieldTable::default())
                        })
                        .then(|r| {
                            assert!(r.is_err());
                            let err = r.unwrap_err();
                            // Seems that it should be some new error kind, like NotFound
                            if let lapin_futures::ErrorKind::PreconditionFailed = err.kind() {
                                Err::<(), _>(err)
                            } else {
                                panic!("Wrong error, expected lapin_futures::ErrorKind::PreconditionFailed, found {}", err.kind());
                            }
                        })
                })
            })
                .map_err(Error::from)
        })
    ).expect("runtime failure");
}
