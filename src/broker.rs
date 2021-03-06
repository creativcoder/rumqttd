use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::collections::{VecDeque, HashMap};
use std::fmt::{self, Debug};

use slog::{Logger, Drain};
use slog_term;
use slog_async;

use mqtt3::*;

use client::Client;

#[derive(Debug)]
pub struct BrokerState {
    /// For QoS 1. Stores incoming publishes
    pub incoming_pub: VecDeque<Box<Publish>>,
    /// For QoS 2. Stores incoming publishes
    pub incoming_rec: VecDeque<Box<Publish>>,
    /// For QoS 2. Stores incoming release
    pub incoming_rel: VecDeque<PacketIdentifier>,
    /// For QoS 2. Stores incoming comp
    pub incoming_comp: VecDeque<PacketIdentifier>,
}

impl BrokerState {
    fn new() -> Self {
        BrokerState {
            incoming_pub: VecDeque::new(),
            incoming_rec: VecDeque::new(),
            incoming_rel: VecDeque::new(),
            incoming_comp: VecDeque::new(),
        }
    }
}

#[derive(Clone)]
pub struct Broker {
    /// All the active clients mapped to their IDs
    clients: Rc<RefCell<HashMap<String, Client>>>,
    /// Subscriptions mapped to interested clients
    subscriptions: Rc<RefCell<HashMap<SubscribeTopic, Vec<Client>>>>,
    pub state: Rc<RefCell<BrokerState>>,
    logger: Logger,
}

impl Broker {
    pub fn new() -> Self {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();

        let state = BrokerState::new();

        Broker {
            clients: Rc::new(RefCell::new(HashMap::new())),
            subscriptions: Rc::new(RefCell::new(HashMap::new())),
            state: Rc::new(RefCell::new(state)),
            logger: Logger::root(Arc::new(drain), o!("version" => env!("CARGO_PKG_VERSION"))),
        }
    }

    /// Adds a new client to the broker
    pub fn add_client(&self, client: Client) {
        self.clients
            .borrow_mut()
            .insert(client.id.clone(), client);
    }

    /// Adds client to a subscription. If the subscription doesn't exist,
    /// new subscription is created and the client will be added to it
    fn add_subscription_client(&self, topic: SubscribeTopic, client: Client) {
        let mut subscriptions = self.subscriptions.borrow_mut();
        let clients = subscriptions.entry(topic).or_insert(Vec::new());

        // add client to a subscription only if it doesn't already exist or
        // else replace the existing one
        if let Some(index) = clients.iter().position(|v| v.id == client.id) {
            clients.insert(index, client);
        } else {
            clients.push(client);
        }
    }

    /// Remove a client from a subscription
    pub fn remove_subscription_client(&self, topic: SubscribeTopic, id: &str) {
        let mut subscriptions = self.subscriptions.borrow_mut();

        if let Some(clients) = subscriptions.get_mut(&topic) {
            if let Some(index) = clients.iter().position(|v| v.id == id) {
                clients.remove(index);
            }
        }
    }

    /// Get the list of clients for a given subscription
    fn get_subscribed_clients(&self, topic: SubscribeTopic) -> Vec<Client> {
        let subscriptions = self.subscriptions.borrow_mut();

        if let Some(v) = subscriptions.get(&topic) {
            v.clone()
        } else {
            vec![]
        }
    }

    // Remove the client from broker (including subscriptions)
    pub fn remove_client(&self, id: &str) {
        self.clients.borrow_mut().remove(id);

        let mut subscriptions = self.subscriptions.borrow_mut();

        for clients in subscriptions.values_mut() {
            if let Some(index) = clients.iter().position(|v| v.id == id) {
                clients.remove(index);
            }
        }
    }

    // TODO: Find out if broker should drop message if a new massage with existing
    // pkid is received
    pub fn store_publish(&self, publish: Box<Publish>) {
        let mut state = self.state.borrow_mut();
        state.incoming_pub.push_back(publish.clone());
    }

    pub fn remove_publish(&self, pkid: PacketIdentifier) -> Option<Box<Publish>> {
        let mut state = self.state.borrow_mut();

        match state
                  .incoming_pub
                  .iter()
                  .position(|x| x.pid == Some(pkid)) {
            Some(i) => state.incoming_pub.remove(i),
            None => None,
        }
    }

    pub fn store_record(&self, publish: Box<Publish>) {
        let mut state = self.state.borrow_mut();
        state.incoming_rec.push_back(publish.clone());
    }

    pub fn remove_record(&self, pkid: PacketIdentifier) -> Option<Box<Publish>> {
        let mut state = self.state.borrow_mut();

        match state
                  .incoming_pub
                  .iter()
                  .position(|x| x.pid == Some(pkid)) {
            Some(i) => state.incoming_rec.remove(i),
            None => None,
        }
    }

    pub fn store_rel(&self, pkid: PacketIdentifier) {
        let mut state = self.state.borrow_mut();
        state.incoming_rel.push_back(pkid);
    }

    pub fn remove_rel(&self, pkid: PacketIdentifier) {
        let mut state = self.state.borrow_mut();

        match state.incoming_rel.iter().position(|x| *x == pkid) {
            Some(i) => state.incoming_rel.remove(i),
            None => None,
        };
    }

    pub fn store_comp(&self, pkid: PacketIdentifier) {
        let mut state = self.state.borrow_mut();
        state.incoming_comp.push_back(pkid);
    }

    pub fn remove_comp(&self, pkid: PacketIdentifier) {
        let mut state = self.state.borrow_mut();

        match state.incoming_comp.iter().position(|x| *x == pkid) {
            Some(i) => state.incoming_comp.remove(i),
            None => None,
        };
    }

    pub fn handle_subscribe(&self, subscribe: Box<Subscribe>, client: &Client) {
        let pkid = subscribe.pid;
        let mut return_codes = Vec::new();

        // Add current client's id to this subscribe topic
        for topic in subscribe.topics {
            self.add_subscription_client(topic.clone(), client.clone());
            return_codes.push(SubscribeReturnCodes::Success(topic.qos));
        }

        let suback = client.suback_packet(pkid, return_codes);
        let packet = Packet::Suback(suback);
        client.send(packet);
    }

    fn forward_to_subscribers(&self, publish: Box<Publish>) {
        let topic = publish.topic_name.clone();
        let payload = publish.payload.clone();

        // publish to all the subscribers in different qos `SubscribeTopic`
        // hash keys
        for qos in [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce].iter() {

            let subscribe_topic = SubscribeTopic {
                topic_path: topic.clone(),
                qos: qos.clone(),
            };

            for client in self.get_subscribed_clients(subscribe_topic) {
                let publish = client.publish_packet(&topic, qos.clone(), payload.clone(), false, false);
                let packet = Packet::Publish(publish.clone());

                match *qos {
                    QoS::AtLeastOnce => client.store_publish(publish),
                    QoS::ExactlyOnce => client.store_record(publish),
                    _ => (),
                }

                client.send(packet);
            }
        }
    }

    pub fn handle_publish(&self, publish: Box<Publish>, client: &Client) {
        let pkid = publish.pid;
        let qos = publish.qos;

        match qos {
            QoS::AtMostOnce => self.forward_to_subscribers(publish),
            // send puback for qos1 packet immediately
            QoS::AtLeastOnce => {
                if let Some(pkid) = pkid {
                    let packet = Packet::Puback(pkid);
                    client.send(packet);
                    // we should fwd only qos1 packets to all the subscribers (any qos) at this point
                    self.forward_to_subscribers(publish);
                } else {
                    error!(self.logger,
                           "Ignoring publish packet. No pkid for QoS1 packet");
                }
            }
            // save the qos2 packet and send pubrec
            QoS::ExactlyOnce => {
                if let Some(pkid) = pkid {
                    self.store_record(publish.clone());
                    let packet = Packet::Pubrec(pkid);
                    client.send(packet);
                } else {
                    error!(self.logger,
                           "Ignoring record packet. No pkid for QoS2 packet");
                }
            }
        }
    }

    pub fn handle_puback(&self, pkid: PacketIdentifier, client: &Client) {
        client.remove_publish(pkid);
    }

    pub fn handle_pubrec(&self, pkid: PacketIdentifier, client: &Client) {
        debug!(self.logger, "PubRec <= {:?}", pkid);

        // remove record packet from state queues
        if let Some(record) = client.remove_record(pkid) {
            // record and send pubrel packet
            client.store_rel(record.pid.unwrap()); //TODO: Remove unwrap. Might be a problem if client behaves incorrectly
            let packet = Packet::Pubrel(pkid);
            client.send(packet);
        }
    }

    pub fn handle_pubcomp(&self, pkid: PacketIdentifier, client: &Client) {
        // remove release packet from state queues
        client.remove_rel(pkid);
    }

    pub fn handle_pubrel(&self, pkid: PacketIdentifier, client: &Client) {
        // client is asking to release all the recorded packets

        // send pubcomp packet to the client first
        let packet = Packet::Pubcomp(pkid);
        client.send(packet);

        if let Some(record) = client.remove_record(pkid) {
            let topic = record.topic_name.clone();
            let payload = record.payload;

            // publish to all the subscribers in different qos `SubscribeTopic`
            // hash keys
            for qos in [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce].iter() {

                let subscribe_topic = SubscribeTopic {
                    topic_path: topic.clone(),
                    qos: qos.clone(),
                };

                for client in self.get_subscribed_clients(subscribe_topic) {
                    let publish = client.publish_packet(&topic, qos.clone(), payload.clone(), false, false);
                    let packet = Packet::Publish(publish.clone());

                    match *qos {
                        QoS::AtLeastOnce => client.store_publish(publish),
                        QoS::ExactlyOnce => client.store_record(publish),
                        _ => (),
                    }

                    client.send(packet);
                }
            }
        }
    }

    pub fn handle_pingreq(&self, client: &Client) {
        let pingresp = Packet::Pingresp;
        client.send(pingresp);
    }
}

impl Debug for Broker {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
               "{:#?}\n{:#?}\n{:#?}",
               self.clients.borrow(),
               self.subscriptions.borrow(),
               self.state.borrow())
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use futures::sync::mpsc::{self, Receiver};
    use client::Client;
    use super::Broker;
    use mqtt3::*;

    fn mock_client(id: &str) -> (Client, Receiver<Packet>) {
        let (tx, rx) = mpsc::channel::<Packet>(8);
        (Client::new(id, "127.0.0.1:80".parse().unwrap(), tx), rx)
    }

    #[test]
    fn add_and_remove_clients_to_the_broker() {
        let (c1, ..) = mock_client("mock-client-1");
        let (c2, ..) = mock_client("mock-client-2");
        let (c3, ..) = mock_client("mock-client-3");

        let broker = Broker::new();
        broker.add_client(c1);
        broker.add_client(c2);
        broker.add_client(c3);

        {
            let clients = broker.clients.borrow();
            assert_eq!(clients.contains_key("mock-client-1"), true);
            assert_eq!(clients.contains_key("mock-client-2"), true);
            assert_eq!(clients.contains_key("mock-client-3"), true);
        }

        broker.remove_client("mock-client-2");

        {
            let clients = broker.clients.borrow();
            assert_eq!(clients.contains_key("mock-client-1"), true);
            assert_eq!(clients.contains_key("mock-client-2"), false);
            assert_eq!(clients.contains_key("mock-client-3"), true);
        }
    }

    #[test]
    fn add_and_remove_subscriptions_to_the_broker() {
        let (c1, ..) = mock_client("mock-client-1");
        let (c2, ..) = mock_client("mock-client-2");

        let s1 = SubscribeTopic {
            topic_path: "hello/mqtt".to_owned(),
            qos: QoS::AtMostOnce,
        };
        let s2 = SubscribeTopic {
            topic_path: "hello/mqtt".to_owned(),
            qos: QoS::AtLeastOnce,
        };
        let s3 = SubscribeTopic {
            topic_path: "hello/mqtt".to_owned(),
            qos: QoS::ExactlyOnce,
        };
        let s4 = SubscribeTopic {
            topic_path: "hello/rumqttd".to_owned(),
            qos: QoS::AtLeastOnce,
        };
        let s5 = SubscribeTopic {
            topic_path: "hello/rumqttd".to_owned(),
            qos: QoS::ExactlyOnce,
        };

        let broker = Broker::new();

        // add c1 to to s1, s2, s3 & s4
        broker.add_subscription_client(s1.clone(), c1.clone());
        broker.add_subscription_client(s2.clone(), c1.clone());
        broker.add_subscription_client(s3.clone(), c1.clone());
        broker.add_subscription_client(s4.clone(), c1.clone());

        // add c2 to s2 & s5
        broker.add_subscription_client(s2.clone(), c2.clone());
        broker.add_subscription_client(s5.clone(), c2.clone());

        // verify clients in s1
        let clients = broker.get_subscribed_clients(s1.clone());
        assert_eq!(clients.len(), 1);
        assert_eq!(clients.get(0).unwrap().id, "mock-client-1");

        // verify clients in s2
        let clients = broker.get_subscribed_clients(s2.clone());
        assert_eq!(clients.len(), 2);
        assert_eq!(clients.get(0).unwrap().id, "mock-client-1");
        assert_eq!(clients.get(1).unwrap().id, "mock-client-2");

        // verify clients in s5
        let clients = broker.get_subscribed_clients(s5.clone());
        assert_eq!(clients.len(), 1);
        assert_eq!(clients.get(0).unwrap().id, "mock-client-2");

        // remove c1 from s2 and verify clients
        broker.remove_subscription_client(s2.clone(), &c1.id);
        let clients = broker.get_subscribed_clients(s2.clone());
        assert_eq!(clients.len(), 1);
        assert_eq!(clients.get(0).unwrap().id, "mock-client-2");

        // remove c1 & c2 from all subscriptions and verify clients
        broker.remove_client(&c1.id);
        broker.remove_client(&c2.id);

        for s in [s1, s2, s3, s4, s5].iter() {
            let clients = broker.get_subscribed_clients(s.clone());
            assert_eq!(clients.len(), 0);
        }

    }

}
