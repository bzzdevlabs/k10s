//! A node with one stack instance per queue, as the kernel runs them per CPU: software RSS steering for a
//! single-queue NIC and the event routing between instances (forwarded frames, interface configuration,
//! neighbors). Tests and benchmarks use it; the kernel does the same over its per-CPU queues.

use crate::buf::{Account, Quota};
use crate::iface::{IfaceConfig, Ipv4Cidr};
use crate::ports::{ListenerConfig, ListenerId, Ports, PortsConfig, ViewId};
use crate::rss::{Rss, DEFAULT_KEY};
use crate::sim::{mac, Endpoint};
use crate::stack::{Event, Stack, StackConfig, TcpHandle, TxSink};
use crate::time::Instant;
use std::net::Ipv4Addr;
use std::sync::Arc;

pub struct Multi {
    pub stacks: Vec<Stack>,
    pub ports: Ports,
    pub rss: Rss,
    pub quota: Arc<Quota>,
    /// Application events (connections, listeners, UDP) with the instance that raised them.
    pub events: Vec<(u16, Event)>,
}

impl Multi {
    /// Host `n` at 10.0.0.n/24 with `queues` instances.
    pub fn new(n: u8, queues: u16, cfg: StackConfig) -> Multi {
        let mut iface = IfaceConfig::new(mac(n), 1500);
        iface.ipv4 = Some(Ipv4Cidr { addr: Ipv4Addr::new(10, 0, 0, n), prefix_len: 24 });
        let iface = Arc::new(iface);
        let ports = Ports::new(PortsConfig::default(), &cfg.node_secret);
        let quota = Arc::new(Quota::new(1 << 30, 1 << 20));
        let rss = Rss::new(DEFAULT_KEY, queues);
        let stacks = (0..queues)
            .map(|q| {
                let c = StackConfig { queue: q, ..cfg.clone() };
                Stack::new(c, rss.clone(), iface.clone(), ports.snapshot(), quota.clone())
            })
            .collect();
        Multi { stacks, ports, rss, quota, events: Vec::new() }
    }

    pub fn listen(&mut self, port: u16, cfg: ListenerConfig) -> ListenerId {
        let acc = Account { global: self.quota.clone(), view: None };
        let l = self.ports.listen(ViewId::NODE, None, port, cfg, acc).unwrap();
        self.publish_ports();
        l.id
    }

    pub fn publish_ports(&mut self) {
        let t = self.ports.snapshot();
        for s in &mut self.stacks {
            s.set_ports(t.clone());
        }
    }

    /// Accepts from whichever instance has a connection queued.
    pub fn accept(&mut self, l: ListenerId) -> Option<TcpHandle> {
        self.stacks.iter_mut().find_map(|s| s.tcp_accept(l))
    }

    pub fn stack(&mut self, h: TcpHandle) -> &mut Stack {
        &mut self.stacks[h.queue as usize]
    }

    /// Routes the instances' events until none is left.
    pub fn pump(&mut self, now: Instant) {
        loop {
            let mut any = false;
            for i in 0..self.stacks.len() {
                while let Some(e) = self.stacks[i].poll_event() {
                    any = true;
                    match e {
                        Event::Forward { queue, frame } => {
                            self.stacks[queue as usize].receive(now, &frame);
                            self.stacks[i].recycle(frame);
                        }
                        Event::Iface(c) => {
                            for (j, s) in self.stacks.iter_mut().enumerate() {
                                if j != i {
                                    s.set_iface(c.clone());
                                }
                            }
                        }
                        Event::Neighbor { ip, mac } => {
                            for (j, s) in self.stacks.iter_mut().enumerate() {
                                if j != i {
                                    s.learn_neighbor(now, ip, mac);
                                }
                            }
                        }
                        e => self.events.push((i as u16, e)),
                    }
                }
            }
            if !any {
                return;
            }
        }
    }
}

impl Endpoint for Multi {
    fn receive(&mut self, now: Instant, frame: &[u8]) {
        let q = self.rss.classify(frame) as usize;
        self.stacks[q].receive(now, frame);
        self.pump(now);
    }
    fn transmit(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        for i in 0..self.stacks.len() {
            self.stacks[i].transmit(now, out as &mut dyn TxSink);
            self.pump(now);
        }
    }
    fn poll_at(&mut self, now: Instant) -> Option<Instant> {
        self.stacks.iter().filter_map(|s| s.poll_at(now)).min()
    }
}
