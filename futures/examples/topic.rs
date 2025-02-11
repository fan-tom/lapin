use env_logger;
use failure::Error;
use futures::Future;
use lapin_futures as lapin;
use crate::lapin::{BasicProperties, Client, ConnectionProperties};
use crate::lapin::options::{BasicPublishOptions, ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions};
use crate::lapin::types::FieldTable;
use tokio;
use tokio::runtime::Runtime;

fn main() {
    env_logger::init();

    let addr = std::env::var("AMQP_ADDR").unwrap_or_else(|_| "amqp://127.0.0.1:5672/%2f".into());
    let runtime = Runtime::new().unwrap();

    runtime.block_on_all(
        Client::connect(&addr, ConnectionProperties::default()).map_err(Error::from).and_then(|client| {
            client.create_channel()
                .and_then(|channel| {
                    channel.clone().exchange_declare("hello_topic", "topic", ExchangeDeclareOptions::default(), FieldTable::default()).map(move |_| channel)
                }).and_then(|channel| {
                    channel.clone().queue_declare("topic_queue", QueueDeclareOptions::default(), FieldTable::default()).map(move |_| channel)
                }).and_then(|channel| {
                    channel.clone().queue_bind("topic_queue", "hello_topic", "*.foo.*", QueueBindOptions::default(), FieldTable::default()).map(move |_| channel)
                }).and_then(|channel| {
                    channel.basic_publish("hello_topic", "hello.fooo.bar", b"hello".to_vec(), BasicPublishOptions::default(), BasicProperties::default())
                })
                .map_err(Error::from)
        }).map_err(|err| eprintln!("An error occured: {}", err))
    ).expect("runtime exited with failure");
}
