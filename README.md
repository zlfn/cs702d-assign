# Orbit: A Ring-Based Protocol for Global Distributed Rate Limiting

Orbit is a global, distributed, rate limiting protocol, inspired by systems such as [Doorman](https://github.com/youtube/doorman), that enforces **strict global caps** on shared resources. It tracks and limits requests to resources such as GPU clusters, external APIs, and databases based on UUID identifiers.

---

## 1. Problem Statement

In a distributed server environment, user requests compete for shared resources (e.g., GPU clusters, external APIs, databases). The system must **accept or reject** each request such that:

- The user’s total usage across the **entire system** never exceeds the global allocation.
- The **global cap is strict**: at no point in time may the aggregated usage exceed the configured limit.
- Resource demands may fluctuate rapidly. Even short-lived high-demand bursts should be allowed as long as sufficient global capacity remains.

Formally, the system must guarantee that the instantaneous sum of consumed capacity over all nodes does not exceed a global cap, while operating in a distributed setting with no single point of control and under realistic network conditions.

Multiple approaches have been proposed to solve this problem; however, each approach has significant drawbacks when we require **global strictness, low latency, and high availability** simultaneously.

---

## 2. Existing Approaches

### 2.1 Static Quota Distribution

One straightforward method is to statically partition the global capacity across all nodes in advance.

- Each node is assigned a fixed quota and independently enforces a local limit.
- Requests are admitted or rejected based solely on the node’s local quota.

However, this approach has substantial limitations:

- It is highly vulnerable to **traffic imbalance**. Nodes with low traffic waste their assigned capacity, while hot nodes become overloaded despite remaining global capacity.
- To preserve the global cap in the worst case, the per-node quota must be set conservatively. As the number of nodes grows, each node’s quota must shrink, leading to **poor overall resource utilization**.

### 2.2 Hash-Based Request Sharding

Another approach is to use Consistent Hashing or Rendezvous Hashing to distribute requests across nodes:

- Each request is mapped to a node based on its UUID.
- Each node is responsible only for rate limiting the UUIDs it owns.

This method smooths load across nodes on average, but:

- If a particular UUID experiences a sudden spike in traffic, the corresponding node can still be overloaded.
- Enforcing a strict global cap across all nodes remains non-trivial, as each node’s view is inherently local.

### 2.3 Centralized Rate Limiting

A common design is to introduce a **centralized rate limiting server**:

- All requests (or at least all rate-limiting decisions) pass through a single central node.
- The central node tracks global usage and enforces the cap exactly.

This design is simple and precise, but it introduces serious issues:

- The central server becomes a **bottleneck** for throughput.
- It is a **single point of failure (SPoF)**.

To mitigate failures, distributed consensus protocols such as Raft can be used for leader election (e.g., Doorman-style designs):

- A leader is elected to act as the central authority.
- Upon leader failure, a new leader is elected.

However:

- There is still effectively **a single logical leader** that manages global quotas.
- During leader re-election and state recovery, the system can encounter **availability issues** and temporarily inconsistent quotas.
- Network round-trip latency to the leader significantly impacts request latency. In globally distributed deployments, the physical distance to the leader directly increases end-to-end tail latency.

#### Token Reservation

To reduce frequent communication with the central server, clients may **reserve tokens**:

- Clients periodically obtain a batch of tokens from the central server.
- Local requests are admitted against the reserved tokens without contacting the server each time.

This approach lowers communication overhead but:

- Reduces the **granularity** and **accuracy** of traffic control.
- Still preserves the central authority as a bottleneck and a single point of failure.

### 2.4 Distributed Agreement and CRDT-Based Approaches

CRDTs (Conflict-free Replicated Data Types) can be used to build distributed counters:

- Each node maintains its own copy of a usage table.
- Nodes periodically exchange and merge tables, converging towards a consistent global state (eventual consistency).

However:

- Due to **eventual** rather than strong consistency, these systems **cannot guarantee strict global caps** at all times.
- Gossip-style synchronization requires substantial network communication, especially as the number of nodes grows.

To address numeric invariants under eventual consistency, **Bounded Counter CRDTs (Escrow)** have been proposed:

- The global capacity is split into tokens distributed across nodes.
- Nodes transfer tokens asynchronously between each other when they run low.

This helps enforce numeric invariants, but:

- Inter-node communication volume can be large.
- Capacity fragmentation leads to inefficiencies and underutilization.

### 2.5 Prediction- and Probability-Based Methods

Algorithms such as CMDRL (Markovian Distributed Rate Limiting) and PBFT (Prediction Based Fair Token Bucket Algorithm) use predictive or probabilistic methods:

- Future usage is estimated using stochastic models.
- Rates are probabilistically throttled to approximate adherence to the global cap.

However:

- When predictions are inaccurate, the system may **overshoot** the global cap.
- Probabilistic guarantees are not suitable for workloads requiring **hard business constraints** (e.g., strict cost ceilings, regulatory limits).

---

## 3. The Orbit Protocol

To address the above limitations, we propose the **Orbit protocol**.

Orbit solves global rate limiting **without a centralized master** and **without probabilistic guarantees**, while providing:

- Strict global caps,
- Bounded and predictable communication patterns, and
- Low local request latency.

### 3.1 High-Level Design

Orbit connects all nodes into a **logical ring**:

- Each node communicates only with its **immediate predecessor** and **immediate successor** in the ring.
- There is **no elected master** or central coordinator.

Each epoch is divided into two phases:

1. **Receive Phase (Pull)**  
   The node pulls the cumulative usage table from its predecessor.
2. **Send Phase (Push)**  
   The node updates the cumulative usage table with its own usage and makes it available to its successor.

Within each epoch:

- Each node tracks UUID-specific usage locally.
- At the beginning (or during) the epoch, a node receives the cumulative usage of all **previous nodes in the ring**.
- Based on this cumulative usage and the global cap, the node computes the remaining capacity available to itself.
- The node admits or rejects incoming requests according to this available capacity, **without contacting other nodes** on a per-request basis.
- The node aggregates its own usage into the cumulative table and passes it onward to the next node.

In effect, the ring maintains a **cumulative (prefix) sum** of usage across nodes, circulating once per epoch.

### 3.2 Key Properties

Orbit exhibits the following properties:

- **Elimination of Central Bottlenecks and SPoF**  
  There is no central server. Each node operates independently, and the topology is inherently decentralized.

- **Strict Global Cap Guarantee**  
  Each node computes its allocable capacity based on the **sum of all prior usage** in the ring. As a result, the total usage across all nodes can never exceed the global cap at any point in time, assuming correct operation of the protocol.

  Unlike CMDRL or PBFT, Orbit does not rely on prediction or probabilistic models; it relies on explicit cumulative accounting.

- **Low Per-Request Latency**  
  Nodes make rate limiting decisions using only local state and last received cumulative information. No inter-node communication is required on the critical path of a single request.

- **Simple Network Topology**  
  Communication is limited to a ring rather than a full mesh. Each node maintains a constant number of connections irrespective of the total number of nodes.

---

## 4. Anticipated Challenges and Mitigation Strategies

### 4.1 Predecessor Dominance (Capacity Monopolization)

**Problem.**  
If a node close to the beginning of the ring consumes most of the available capacity, downstream nodes may see very little remaining capacity and effectively starve.

**Mitigation.**  
Introduce a configurable **`rollover_ratio`**:

- Each node reserves a fixed percentage of the remaining capacity for downstream nodes.
- For example, if a node observes `remaining_capacity` and `rollover_ratio = r`, it may only consume up to `(1 - r) * remaining_capacity`, reserving `r * remaining_capacity` for subsequent nodes.

This creates a trade-off:

- Lower `rollover_ratio`  
  → Higher utilization but greater risk of capacity monopolization by early nodes.
- Higher `rollover_ratio`  
  → Improved fairness and more balanced capacity distribution, but potential decrease in overall utilization.

### 4.2 Gaps Between Epochs

**Problem.**  
Between epochs, or before a node has received updated information from its predecessor, it may not know exactly how much global capacity remains. Naively, this could force the node to stall all traffic until the next cumulative update arrives.

**Mitigation.**

- Each node maintains a **local fixed window** allowance for each epoch.
- Even without updated information from the predecessor, a node can admit requests up to this local allowance.
- When an epoch boundary is reached:
  - If local allowance is not fully used, the remaining local budget is carried forward as a local buffer.
  - If there is no remaining allowance and updated information is not yet available, the node may temporarily wait until it receives the new cumulative usage table.

This mechanism smooths out brief communication delays while still preserving overall strictness over the configured time scales.

### 4.3 Node Failures and Network Partitions

**Problem.**  
If a node fails or becomes unreachable, the ring structure is temporarily broken.

**Mitigation.**

- Orbit implements a **Virtual Ring**:
  - If a node cannot obtain usage information from its immediate predecessor, it retries with the predecessor’s predecessor, recursively, effectively skipping failed nodes.
  - Each node sends information to exactly one successor; when links fail, the ring is re-routed dynamically to maintain a single successor per node.

- If a node cannot obtain information from any predecessor (e.g., in a severe partition):
  - The node falls back to using **only its local fixed window** allowance.
  - In that epoch, it does not forward any cumulative information downstream.

This design allows the system to degrade gracefully under partial failures while avoiding uncontrolled overshoot of the global cap.

### 4.4 Large Usage Tables

**Problem.**  
When the number of active users per epoch is very large (e.g., 100,000–1,000,000 UUIDs), maintaining and transmitting full usage tables per epoch can be expensive:

- Serialization and deserialization overhead,
- Network bandwidth consumption.

**Mitigation.**

Fundamentally, any protocol that enforces a strict global cap requires all nodes to share sufficient information about global usage. This imposes a lower bound on the amount of state that must circulate.

However, the practical overhead can be mitigated through:

- Efficient binary formats that avoid repeated serialization overhead, such as **Cap’n Proto**, **FlatBuffers**, or **Rkyv** (zero-copy serialization).
- Incremental or delta-encoding of usage tables (e.g., only transmitting entries that changed in the last epoch).
- Exploiting typical datacenter network characteristics: for example, in a deployment with 1 million users and 10 nodes, approximately 100 MB of data circulating per epoch is often acceptable in a modern datacenter environment.

---

## 5. Limitations of the Protocol

Despite its advantages, Orbit has inherent limitations that arise from fundamental constraints in distributed systems.

### 5.1 Minimum Effective Rate Limit Window

Orbit’s minimal effective rate limiting window is bounded by:

> **`epoch_duration × number_of_nodes`**

That is, Orbit can guarantee that **average usage** over relatively long intervals (e.g., minutes to hours) does not exceed the configured thresholds, but it cannot prevent extremely short-term bursts at a finer granularity than this bound.

The underlying reason is fundamental:

- Before processing a request, a node cannot possess an up-to-date view of all other nodes’ instantaneous state unless continuous, low-latency synchronization is performed—which is often impractical.
- Therefore, Orbit alone cannot fully solve ultra-short timescale rate limiting.

**Mitigation.**

- Use Orbit as a **global, long-term** rate limiter.
- Complement it with **local rate limiting algorithms** (e.g., token bucket, leaky bucket) on each node:
  - Orbit enforces global constraints over longer windows.
  - Local limiters suppress short, high-frequency bursts.

### 5.2 Short-Term Consistency vs. Network Delay

Orbit assumes that each node acts based on cumulative information that might be slightly stale within an epoch. While the protocol is designed so that the global cap is never exceeded over the configured time window, **instantaneous microsecond-scale strictness** is not guaranteed when network latency fluctuates.

### 5.3 Fundamental Partition Limitations

In the presence of severe network partitions, no distributed protocol can simultaneously guarantee:

- Strict global caps,
- High availability,
- Full partition tolerance.

Orbit biases toward safety: when missing upstream information, nodes fall back to local fixed windows only, which can limit throughput but avoids uncontrolled resource overshoot.

---

## 6. Related Work

- [Doorman – Global Distributed Client Side Rate Limiting](https://github.com/youtube/doorman)  
  YouTube’s centralized rate limiting service using a leader-based design.

- [High-throughput distributed rate limiter](https://engineering.linecorp.com/en/blog/high-throughput-distributed-rate-limiter)  
  Engineering notes from LINE on building high-throughput rate limiters.

- [Extending Eventually Consistent Cloud Databases for Enforcing Numeric Invariants](https://ieeexplore.ieee.org/abstract/document/7371565)  
  Discusses Bounded Counter CRDTs (Escrow techniques) for numeric invariants under eventual consistency.

- [CMDRL: A Markovian Distributed Rate Limiting Algorithm in Cloud Networks](https://dl.acm.org/doi/abs/10.1145/3663408.3663417)  
  Proposes a Markovian model-based distributed rate limiting algorithm with probabilistic guarantees.

---

## 7. Conclusion

Orbit introduces a **ring-based, master-less protocol** for global distributed server-side rate limiting that:

- Eliminates central bottlenecks and single points of failure,
- Provides **strict global cap** guarantees without probabilistic approximations,
- Achieves low per-request latency by making decisions locally,
- Maintains a simple and scalable communication topology, and
- Degrades gracefully under node failures and network partitions.

By combining Orbit with local rate limiting at each node, systems can enforce both **global long-term constraints** and **short-term burst control**, making Orbit particularly suitable for:

- GPU clusters and inference workloads,
- External APIs with strict cost or quota constraints,
- Databases and other shared backend resources where overshoot is unacceptable,
- Globally distributed systems where centralized rate limiting is operationally expensive.

Orbit thus occupies a design space between centralized leader-based systems and purely eventually consistent approaches, offering a practical and robust solution for strict, global rate limiting in large-scale distributed environments.
