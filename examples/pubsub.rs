use env_logger;
use lapin;
use log::info;

use crate::lapin::{
  BasicProperties, Channel, Connection, ConnectionProperties, ConsumerDelegate,
  message::Delivery,
  options::*,
  types::FieldTable,
};

#[derive(Clone,Debug)]
struct Subscriber {
  channel: Channel,
}

impl ConsumerDelegate for Subscriber {
  fn on_new_delivery(&self, delivery: Delivery) {
    self.channel.basic_ack(delivery.delivery_tag, BasicAckOptions::default()).as_error().expect("basic_ack");
  }
}

fn main() {
  env_logger::init();

  let addr = std::env::var("AMQP_ADDR").unwrap_or_else(|_| "amqp://127.0.0.1:5672/%2f".into());
  let conn = Connection::connect(&addr, ConnectionProperties::default()).wait().expect("connection error");

  info!("CONNECTED");

  let channel_a = conn.create_channel().wait().expect("create_channel");
  let channel_b = conn.create_channel().wait().expect("create_channel");

  channel_a.queue_declare("hello", QueueDeclareOptions::default(), FieldTable::default()).wait().expect("queue_declare");
  let queue = channel_b.queue_declare("hello", QueueDeclareOptions::default(), FieldTable::default()).wait().expect("queue_declare");

  info!("will consume");
  channel_b.clone().basic_consume(&queue, "my_consumer", BasicConsumeOptions::default(), FieldTable::default()).wait().expect("basic_consume").set_delegate(Box::new(Subscriber { channel: channel_b }));

  let payload = b"Hello world!";

  loop {
    channel_a.basic_publish("", "hello", BasicPublishOptions::default(), payload.to_vec(), BasicProperties::default()).wait().expect("basic_publish");
  }
}

