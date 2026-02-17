Comprehensive Design and Validation Framework for Containerized 4G/5G Network Simulation Environments
1. Introduction and Architectural Objectives
The rapid evolution of mobile telecommunications from 4G LTE to 5G New Radio (NR) has fundamentally altered the landscape of real-time media transport. The promise of 5G—characterized by enhanced Mobile Broadband (eMBB), Ultra-Reliable Low-Latency Communication (URLLC), and massive Machine-Type Communications (mMTC)—suggests a future where high-fidelity media streaming is ubiquitous and resilient. However, the physical reality of cellular deployment introduces complex stochastic pathologies that challenge standard transport protocols. Phenomena such as millimeter-wave (mmWave) blockage, rapid signal fading, bufferbloat in the Radio Link Control (RLC) layer, and inter-Radio Access Technology (RAT) handovers create a network environment defined by high variability. In response, modern media transport protocols increasingly rely on bonding (link aggregation) and multi-homing strategies to distribute flows across multiple heterogenous links (e.g., LTE and 5G simultaneously) to ensure Quality of Experience (QoE).
Validating these bonded protocols requires a rigorous testing environment. Field testing, while realistic, lacks reproducibility and scalability; driving a test vehicle through a city center provides valuable data but cannot be seamlessly integrated into a Continuous Integration (CI) pipeline. Conversely, traditional simulation often relies on simplified statistical models that fail to capture the correlated nature of cellular impairments. The industry requirement is a "Digital Twin" of the cellular environment—a simulation infrastructure that runs entirely within containerized systems (Docker), enabling deterministic, reproducible, and scalable testing of transport logic against realistic 4G/5G channel dynamics.
This report presents an exhaustive research analysis and architectural blueprint for constructing such a simulation environment. It synthesizes methodologies for emulating cellular link behaviors using Linux kernel primitives and higher-order simulators, evaluates the fidelity of competing emulation engines, and details the integration of empirical datasets from 3GPP, NYU Wireless, and MONROE. Furthermore, it addresses the critical engineering challenges of achieving network isolation within Docker, implementing dynamic impairment control via tc and eBPF, and validating simulation accuracy through precise clock synchronization and One-Way Delay (OWD) measurement.
2. Comparative Analysis of Network Emulation Technologies
The selection of the underlying emulation engine is the foundational decision in designing a cellular testbed. The landscape of network simulation tools ranges from discrete-event simulators that model individual electron interactions to kernel-level emulators that manipulate packet queues. For a UDP-based bonded media transport protocol, the primary requirement is accurately replicating the manifestation of physical layer (PHY) events at the network layer (Layer 3) and transport layer (Layer 4), while maintaining compatibility with containerized workflows.
2.1. Linux Traffic Control (tc) and NetEm
Linux Traffic Control (tc) combined with the Network Emulator (netem) queuing discipline represents the de facto standard for link emulation in Linux-based environments. tc allows the user to configure the kernel's packet scheduler, defining how packets are queued, shaped, and transmitted on a network interface.
Mechanism and Capabilities netem provides high utility for transport-layer testing because it transparently intercepts IP packets within the kernel's egress path without requiring modifications to the application binary. It allows for the injection of delay, packet loss, duplication, and reordering. Crucially for cellular modeling, netem supports correlated loss models. Cellular packet loss is rarely an independent Bernoulli process; rather, it occurs in bursts due to deep fading or handover failures. netem implements the Gilbert-Elliott model, a 2-state Markov chain (Good/Bad states) that can statistically replicate these burst patterns.1 Furthermore, netem allows delay distributions to be defined by mathematical functions. While simple wired networks might be modeled with a normal distribution, cellular jitter is heavily influenced by retransmission mechanisms (HARQ/ARQ), resulting in heavy-tailed distributions. netem supports Pareto and Pareto-Normal distributions, which effectively model the "long tail" latency spikes observed in loaded cellular cells.3
Docker Compatibility tc is natively compatible with Docker, as Docker containers are processes isolated by namespaces, including the network namespace. tc rules can be applied to the virtual Ethernet (veth) interfaces that connect a container to a Docker bridge. This allows for per-container impairment. However, applying tc rules requires the container to possess the NET_ADMIN capability (--cap-add=NET_ADMIN), or for the rules to be applied from the host context onto the container's virtual interface.5 The ability to script tc commands allows it to be driven by external controllers, making it highly suitable for dynamic scenarios.
Limitations The primary limitation of tc is its lack of intrinsic awareness of the radio environment. It treats the link as a pipe with defined statistical properties, unaware of SINR (Signal-to-Interference-plus-Noise Ratio) or Modulation and Coding Schemes (MCS). Additionally, basic netem uses a First-In-First-Out (FIFO) queue, whereas real 5G base stations (gNodeBs) use complex schedulers like Proportional Fair (PF) or Round Robin. This can lead to inaccuracies in how congestion is modeled if not combined with Hierarchical Token Bucket (HTB) hierarchies to simulate queue limits and bandwidth constraints.7
2.2. ns-3 (Network Simulator 3)
ns-3 is a discrete-event network simulator that models network components from the physical layer up to the application layer with high fidelity. It is the academic standard for wireless research, particularly through its LTE (LENA) and 5G mmWave modules.
Cellular Fidelity ns-3 offers the highest fidelity for cellular modeling among open-source tools. It simulates the entire protocol stack, including the Radio Resource Control (RRC), Packet Data Convergence Protocol (PDCP), RLC, MAC, and PHY layers. It can model beamforming vectors, propagation losses (using 3GPP 38.901 models), and the specific behavior of Hybrid ARQ (HARQ) processes.8 This allows ns-3 to simulate complex interactions, such as how a specific scheduling algorithm affects UDP jitter during a handover, or how mmWave blockage affects TCP ramp-up.
Docker Integration Challenges While ns-3 can be run inside a Docker container, integrating it as a real-time emulator for external applications is complex and computationally expensive. ns-3 must be run in "Real-Time Emulation" mode, where the simulation clock is synchronized with the system wall clock. It uses mechanisms like TapBridge or FdNetDevice to capture packets from a Linux virtual interface, process them through the simulated wireless medium, and reinject them.10 The simulation of the PHY layer is extremely CPU-intensive. If the simulation complexity exceeds the CPU's capacity to process events in real-time, the simulation clock drifts from the wall clock, rendering transport layer timers (e.g., TCP RTO) invalid. This "synchronization drift" makes ns-3 risky for high-throughput media testing in a containerized environment, where CPU resources might be constrained or shared.11
2.3. CORE, Mininet, and Kathará
CORE (Common Open Research Emulator) CORE sits between simulation and emulation. It uses Linux network namespaces to instantiate lightweight virtual nodes (similar to Docker) but typically relies on netem for link effects. Its graphical user interface (GUI) and Python API allow for the rapid creation of topologies. While it natively supports basic link effects (bandwidth, delay), it does not inherently model cellular PHY dynamics better than raw tc. Its integration with EMANE (Extendable Mobile Ad-hoc Network Emulator) provides higher fidelity for radio models, but EMANE is complex to configure and primarily targets MANETs rather than cellular infrastructure.12
Mininet Mininet is designed primarily for Software-Defined Networking (SDN) research using OpenFlow. It creates a network of virtual hosts, switches, and controllers. While a fork named Mininet-WiFi exists, which adds wireless channel modeling 13, standard Mininet relies on tc for link bandwidth and delay. Its focus on switching logic makes it less relevant for point-to-point cellular link fidelity compared to tools dedicated to link emulation.
Kathará Kathará is a network emulation system based on Docker containers, focusing heavily on routing protocols (BGP, OSPF). It provides an excellent orchestration layer for defining network topologies using a simple text-based format. However, like CORE, it leverages standard Linux networking primitives for the data plane. It does not introduce new cellular modeling capabilities beyond what is available via tc or ns-3 integration.14
2.4. Synthesis and Recommendation
For testing a UDP-based bonded media transport protocol, the transport logic is sensitive to packet arrival times, loss patterns, and capacity variations, but not necessarily to the internal state machine of the cellular modem (unless that state is exposed to the OS). Therefore, the recommended architecture is a hybrid approach using Docker and tc netem driven by high-fidelity traces, rather than a full PHY simulator like ns-3.
ns-3 provides superior accuracy but introduces significant architectural complexity and performance bottlenecks that hinder the "real-time" aspect of media transport testing. tc netem, when configured with advanced distributions (Pareto) and driven by empirical data (Mahimahi traces), offers a "grey-box" model that is sufficiently accurate for transport layer validation while remaining lightweight enough to scale in a Docker Compose environment.
3. Realistic Cellular Link Models and Datasets
To utilize tc netem effectively, it must be parameterized with data that reflects the reality of 4G and 5G networks. Using generic parameters (e.g., "100ms delay, 1% random loss") fails to capture the pathologies that break bonded protocols.
3.1. Cellular Link Characteristics
Variable Bandwidth and Scheduling Cellular bandwidth is not constant; it is slotted. The base station scheduler allocates Resource Blocks (RBs) to users every Transmission Time Interval (TTI)—1ms for LTE, and potentially much lower (e.g., 0.125ms) for 5G NR depending on the numerology.15 This creates a "staircase" function for available bandwidth. Furthermore, 5G mmWave links are susceptible to blockage events (e.g., a hand covering the antenna or a building obstructing the Line of Sight), which can cause throughput to drop from multi-gigabit speeds to near zero in milliseconds.16
Correlated (Bursty) Loss Packet loss in cellular networks is predominantly correlated. The Gilbert-Elliott model captures this behavior using a two-state Markov chain: a "Good" state with low loss probability and a "Bad" state with high loss probability (representing deep fading or interference).1
Parameters: The model is defined by transition probabilities  (Good to Bad) and  (Bad to Good), and error probabilities within each state ( for Good,  for Bad).18
Implication: A bonded protocol must be tested against burst losses that exceed the length of its Forward Error Correction (FEC) recovery window.
Heavy-Tailed Jitter (Bufferbloat) Cellular networks employ aggressive retransmission mechanisms at the link layer (HARQ/ARQ) to ensure reliability. This recovery process introduces variable latency. When the link is loaded, queues build up (bufferbloat), leading to RTTs that can exceed 500ms before a packet is dropped. Empirical measurements show that jitter distributions in loaded cells follow Pareto or Log-Normal distributions, characterized by a "heavy tail" of high-latency packets.3
Handoff Dynamics
Handoffs (HO) are critical events for mobile media.
Intra-frequency HO: Typically seamless but adds processing delay.
Inter-RAT HO (e.g., 5G to 4G): Can introduce a "break" in connectivity ranging from 30ms to several seconds depending on the core network integration (N26 interface) and signal conditions. This often results in a burst of packet loss followed by a burst of out-of-order packets as buffered data is flushed from the source base station to the target.20
3.2. Recommended Datasets
To drive the simulation, researchers should utilize published traces rather than synthetic distributions whenever possible.
NYU Wireless Datasets: This repository is critical for 5G mmWave modeling. NYU Wireless provides extensive channel impulse response data for 28 GHz, 73 GHz, and 142 GHz bands. These datasets capture the unique propagation characteristics of mmWave, including rapid signal attenuation due to foliage or human blockage. They are essential for testing how a bonded protocol reacts to the sudden, drastic capacity drops characteristic of high-frequency 5G.16
MONROE (Measuring Mobile Broadband Networks in Europe): The MONROE project offers an open-access database of large-scale measurements from multi-homed nodes on buses and trains across Europe. These traces provide realistic profiles of bandwidth variability, delay, and carrier aggregation behavior in high-mobility scenarios. They are particularly valuable for defining the "service curves" for trace replay in mobile contexts.16
3GPP Channel Models (TR 38.901): For analytic rigor, the 3GPP standards define statistical channel models such as Clustered Delay Line (CDL) and Tapped Delay Line (TDL) for various environments (Urban Micro, Urban Macro, Indoor). While difficult to map directly to tc parameters, they serve as the ground truth for validating synthetic trace generators built in ns-3.11
Pantheon and FCC Data:
Pantheon: A dataset and testbed from Stanford University that focuses on congestion control. It contains calibrated traces from diverse paths (cellular, satellite, WiFi) and is specifically designed to test transport protocols.25
FCC Broadband Data: Provides macroscopic throughput data but often lacks the millisecond-level granularity required for jitter simulation. It is useful for setting baseline capacity expectations for rural vs. urban scenarios.25
3.3. Implementation Strategy: Trace Interpolation
A key insight from the CellReplay research is that simple trace replay is insufficient because it assumes link capacity is independent of offered load. In reality, schedulers allocate fewer RBs if the queue is empty. To rigorously test a bonded protocol, the simulation should utilize a dual-trace interpolation method (recording both "light" and "heavy" traffic profiles) or ensure the test traffic saturates the emulated link to match the conditions under which the trace was recorded.26
4. Docker Networking Architecture for Multi-Link Simulation
Simulating a multi-homed device in a single Docker container requires careful manipulation of Linux networking primitives. The container must perceive multiple distinct network interfaces, each routing through a separate emulated path with independent impairment characteristics.
4.1. Isolation Primitives: Namespaces and veth Pairs
Docker containers rely on Linux network namespaces (netns) for isolation. A virtual Ethernet (veth) pair acts as a pipe connecting the container's namespace to the host's root namespace.
Architecture: For a bonded simulation, the "Client" container requires two veth pairs. One end of pair A acts as eth0 (simulated 4G), and one end of pair B acts as eth1 (simulated 5G). The host-side ends of these pairs connect to separate Docker bridges.
Implication: By separating the flows onto distinct bridges, we establish independent control points. Traffic shaping applied to the host-side interface of veth_A will affect only the 4G path, while rules on veth_B affect only the 5G path.28
4.2. Network Drivers: Bridge vs. Macvlan vs. Ipvlan
The choice of Docker network driver dictates how packets traverse the stack and where impairments can be applied.
Driver
Description
Isolation
Impairment Suitability
Recommendation
Bridge
Connects containers via a virtual switch on the host. Uses NAT.
Medium
High. tc rules can be easily attached to the host-side veth interface representing the specific container link.
Primary Choice
Macvlan
Assigns a MAC address to the container, making it appear as a physical device on the network.
Low (L2 direct)
Low. Traffic bypasses the host's standard routing stack, making it difficult to intercept with tc on the host side. Egress shaping must be done inside the container.
Use only for external hardware integration.
Ipvlan
Similar to Macvlan but shares the host MAC. L2 and L3 modes.
High (L3)
Medium. Complex to shape ingress traffic. L3 mode provides excellent isolation but complicates broadcast discovery.
Alternative if Bridge performance is insufficient.

Decision: The User-Defined Bridge driver is the optimal choice for a purely software-based simulation. It provides visibility of the container's interface on the host (as a veth device), enabling the "Network Controller" to apply impairments from the outside without requiring the container to manage its own shaping.28
4.3. Routing Configuration and Policy-Based Routing (PBR)
A significant challenge in multi-homed containers is the default route dilemma. Linux typically installs a single default gateway. If eth0 is the default, traffic sourced from the IP of eth1 might still attempt to leave via eth0 (asymmetric routing) or be dropped by Reverse Path Filtering (rp_filter).
Solution: Policy-Based Routing (PBR) To ensure true path separation, PBR must be configured inside the Client container. This ensures that traffic generated by the bonded protocol on the "5G interface" stays on the "5G network".30 The configuration requires:
Creating separate routing tables (e.g., 100 for 4G, 101 for 5G).
Adding rules to lookup the table based on the source IP address.
Defining default routes within those specific tables.

Bash


# Example PBR Logic
ip route add default via 172.20.0.1 dev eth0 table 100
ip rule add from 172.20.0.2/32 lookup 100
ip route add default via 172.21.0.1 dev eth1 table 101
ip rule add from 172.21.0.2/32 lookup 101


5. Dynamic Impairment Control
Static shaping is insufficient for validating adaptive bonding algorithms. The simulation must dynamically vary conditions to replicate fading, mobility, and congestion.
5.1. Control Mechanisms
1. Scripted Traffic Control (tc)
A simple loop (Bash or Python) running in a "sidecar" container with NET_ADMIN privileges. It reads a trace file and issues tc commands to update the qdisc parameters.
Pros: Simple, dependency-free.
Cons: High overhead. Forking a shell for every update limits granularity to ~10-50ms.
Use Case: Simulating slow-fading channels or "driving down the highway" scenarios.5
2. Pumba
Pumba is a chaos testing tool for Docker that wraps tc. It allows defining "chaos experiments" (e.g., "add 100ms delay to container X for 5 minutes").
Limitations: Pumba is designed for discrete fault injection (chaos engineering) rather than continuous, high-fidelity trace replay. It lacks the ability to smoothly interpolate bandwidth based on a CSV profile.34
3. Toxiproxy
Toxiproxy is a user-space TCP proxy.
Critique: While easy to use, Toxiproxy operates at Layer 4. It breaks the packet timing characteristics essential for UDP media transport analysis. It introduces artificial context-switching latency and cannot simulate low-level packet corruption or the specific queue dynamics of a cellular buffer. It is not recommended for this specific use case.35
4. eBPF (Extended Berkeley Packet Filter)
eBPF represents the state-of-the-art for high-performance impairment. By attaching an eBPF program to the TC egress hook or XDP (eXpress Data Path) hook, one can manipulate packets with nanosecond precision and zero user-space switching overhead.
Implementation: An eBPF map is populated with the trace profile. The kernel-resident program checks the current timestamp against the map and decides whether to drop, delay, or pass the packet.
Advantage: This is the only method capable of simulating 5G mmWave slot-level scheduling (sub-millisecond granularity) without CPU saturation.36
5.2. Implementing Trace Replay (Mahimahi Adaptation)
Mahimahi is the gold standard for trace replay.38 However, its architecture (wrapping shells) is awkward in Docker.
Adaptation: The recommended approach is to "port" Mahimahi's logic. Parse standard Mahimahi trace files (which list timestamps of packet delivery opportunities) and convert them into a time-series of bandwidth limits.
Execution: Use a Python script utilizing the pyroute2 library (which talks directly to the kernel via Netlink, bypassing tc shell overhead) to update a Token Bucket Filter (TBF) or HTB qdisc dynamically. This enables millisecond-level updates suitable for 5G simulation.38
6. Measurement and Validation Tools
A simulation is worthless without validation. Verifying that the configured impairments are actually being applied—and accurately measuring the protocol's response—is critical.
6.1. Clock Synchronization and OWD
Measuring One-Way Delay (OWD) is crucial for detecting asymmetric congestion (e.g., uplink bufferbloat). This requires tight clock synchronization between the Sender and Receiver containers.
The Problem: Docker containers share the host kernel's clock, but heavy CPU load can cause scheduling jitter that looks like network jitter.
Solution:
PTP (Precision Time Protocol): Run ptp4l (Linux PTP) on the host to sync the system clock to a master reference. Map the physical hardware clock (/dev/ptp0) into the container if possible.
Chrony: Run a local Stratum 1 NTP server (using Chrony) in a dedicated container. Configure Client and Server containers to sync solely to this local reference. This eliminates internet jitter and provides sub-millisecond accuracy, which is sufficient for characterizing cellular jitter (typically 10-100ms).39
6.2. Validation Instrumentation
OWAMP (One-Way Active Measurement Protocol): Use the perfsonar/owamp container to run background OWD measurements parallel to the media stream. This provides a "ground truth" baseline of the emulated link quality.41
MACE (Measurement and Analysis for Container Environments): MACE is a specialized tool designed to profile the virtualization overhead of the container environment itself, allowing researchers to subtract the "Docker tax" from their latency measurements.42
7. Concrete Docker Compose Architecture
The following architecture implements a dual-link (4G + 5G) simulation with dynamic impairment capabilities.
7.1. Directory Structure



.
├── docker-compose.yml
├── traces/
│   ├── nyu_5g_mmwave.csv
│   └── monroe_4g_bus.csv
└── scripts/
    ├── setup_routing.sh
    ├── traffic_shaper.py
    └── run_test.sh


7.2. Docker Compose Configuration

YAML


version: "3.8"

services:
  # -------------------------------------------------------
  # Local NTP Master (Chrony) for Clock Sync
  # -------------------------------------------------------
  ntp_master:
    image: cturra/ntp
    container_name: sim_ntp
    cap_add:
      - SYS_TIME
    ports:
      - "123:123/udp"

  # -------------------------------------------------------
  # The Receiver (Cloud Media Server)
  # -------------------------------------------------------
  receiver:
    image: ubuntu:22.04
    container_name: bond_receiver
    cap_add:
      - NET_ADMIN   # Required for policy routing and tc
      - SYS_TIME    # Required for clock sync
    sysctls:
      - net.ipv4.ip_forward=1
    networks:
      wan_4g:
        ipv4_address: 172.20.0.10
      wan_5g:
        ipv4_address: 172.21.0.10
    volumes:
      -./scripts:/scripts
    command: >
      bash -c "/scripts/setup_routing.sh && 
               chronyd -d -s 'server sim_ntp iburst' &&
               iperf3 -s"

  # -------------------------------------------------------
  # The Sender (Mobile Client)
  # -------------------------------------------------------
  sender:
    image: ubuntu:22.04
    container_name: bond_sender
    depends_on:
      - receiver
      - ntp_master
    cap_add:
      - NET_ADMIN
      - SYS_TIME
    networks:
      wan_4g:
        ipv4_address: 172.20.0.20
      wan_5g:
        ipv4_address: 172.21.0.20
    volumes:
      -./scripts:/scripts
      -./traces:/traces
    command: >
      bash -c "/scripts/setup_routing.sh &&
               chronyd -d -s 'server sim_ntp iburst' &&
               /scripts/run_test.sh"

  # -------------------------------------------------------
  # Network Controller (Impairment Sidecar)
  # -------------------------------------------------------
  net_controller:
    image: python:3.9-slim
    container_name: net_controller
    network_mode: "host" # Needs host access to shape veths
    privileged: true     # Required to manipulate host tc
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      -./scripts:/scripts
      -./traces:/traces
    # Install dependencies and run shaper
    command: >
      bash -c "pip install docker pyroute2 && 
               python3 /scripts/traffic_shaper.py"

networks:
  wan_4g:
    name: sim_4g_bridge
    driver: bridge
    ipam:
      config:
        - subnet: 172.20.0.0/24
  wan_5g:
    name: sim_5g_bridge
    driver: bridge
    ipam:
      config:
        - subnet: 172.21.0.0/24


7.3. Key Script Implementations
1. Policy-Based Routing (setup_routing.sh)
This script must run inside both Sender and Receiver to ensure traffic adheres to the separate links.

Bash


#!/bin/bash
# Enable two routing tables
echo "100 lte_table" >> /etc/iproute2/rt_tables
echo "101 5g_table" >> /etc/iproute2/rt_tables

# ETH0 (4G) Rules
# Lookup table 100 if traffic comes from eth0 IP
ip rule add from 172.20.0.0/24 lookup 100
# Define default route for table 100 via Docker gateway
ip route add default via 172.20.0.1 dev eth0 table 100

# ETH1 (5G) Rules
ip rule add from 172.21.0.0/24 lookup 101
ip route add default via 172.21.0.1 dev eth1 table 101

# Flush cache
ip route flush cache


2. Dynamic Shaper (traffic_shaper.py)
This Python script runs on the host (via the sidecar) and updates the host-side veth interfaces.

Python


import time
import csv
import docker
from pyroute2 import IPRoute

ip = IPRoute()
client = docker.from_env()

def get_host_veth(container_name, internal_iface):
    """
    Resolves the container's internal ethX to the host's vethYYYY.
    Requires executing ip link inside container to get iflink index.
    """
    con = client.containers.get(container_name)
    # Get the iflink index of the interface inside container
    cmd = f"cat /sys/class/net/{internal_iface}/iflink"
    iflink_id = con.exec_run(cmd).output.decode().strip()
    
    # Find the host interface with this index
    for link in ip.get_links():
        if str(link['index']) == iflink_id:
            return link.get_attr('IFLA_IFNAME')
    return None

def apply_netem(iface, rate, delay, jitter, loss):
    """
    Applies TC Netem rules. 
    Uses Gilbert-Elliott for loss and Pareto for jitter.
    """
    # Note: Constructing the netlink message directly is more efficient
    # but shell commands are shown here for clarity.
    cmd = (f"tc qdisc replace dev {iface} root netem "
           f"rate {rate}mbit "
           f"delay {delay}ms {jitter}ms distribution pareto "
           f"loss gemodel {loss}% 10% 0.1% 0.1%") # Simplified GE params
    
    # In a real deployment, use pyroute2's tc() methods to avoid fork overhead
    import subprocess
    subprocess.run(cmd, shell=True)

def run_replay():
    # Resolve interfaces
    veth_4g = get_host_veth("bond_sender", "eth0")
    veth_5g = get_host_veth("bond_sender", "eth1")
    
    # Load Trace
    with open('/traces/nyu_5g_mmwave.csv') as f:
        reader = csv.DictReader(f)
        for row in reader:
            # Apply to 5G Link
            apply_netem(veth_5g, 
                        rate=row['bandwidth'], 
                        delay=row['latency'], 
                        jitter=row['jitter'], 
                        loss=row['loss'])
            
            # (Optional) Apply separate trace to 4G link
            
            time.sleep(0.1) # 100ms granularity

if __name__ == "__main__":
    run_replay()


8. Second and Third-Order Analytical Insights
Developing a high-fidelity simulation reveals deeper systemic interactions that affect media transport.
The "Observer Effect" in Container Networking
Updating tc rules involves kernel locking. When simulating high-frequency channel variations (e.g., 5G slot scheduling at <10ms intervals), the overhead of the "Network Controller" issuing netlink commands can saturate the host CPU's interrupt handling. This introduces unintended scheduling latency for the application containers, artificially skewing OWD measurements.
Mitigation: Use batch mode for tc or migrate the shaper logic to eBPF maps, which can be updated atomically from user space without reloading the entire qdisc.
Correlated Failure Modes
In bonded scenarios, researchers often model links as statistically independent. However, real-world cellular deployments often share backhaul infrastructure or physical towers. A congestion event in the backhaul affects both LTE and 5G simultaneously.
Implication: The simulation traces should include periods of correlated degradation to verify that the bonding protocol does not incorrectly assume that "if Link A is bad, Link B must be good."
TCP vs. UDP Interaction
While the bonded protocol is UDP-based, the simulated link likely carries background traffic in the real world. Cellular "Bufferbloat" is largely driven by TCP congestion control filling the RLC buffers.
Implication: A realistic simulation should not just limit bandwidth but also inject background TCP flows (using iperf in parallel) into the same emulated bridge. This forces the bonding protocol to contend for queue space, testing its Active Queue Management (AQM) sensitivity.
9. Conclusion
The construction of a Docker-based simulation environment for bonded media transport is a viable and powerful alternative to expensive hardware emulators. By leveraging the isolation of Linux namespaces and the shaping power of tc netem, researchers can create a reproducible testbed that runs on standard commodity hardware. However, fidelity relies on moving beyond simple Gaussian models. The integration of Gilbert-Elliott loss models, Pareto jitter distributions, and trace-driven bandwidth profiles derived from datasets like NYU Wireless and MONROE is non-negotiable for capturing the harsh reality of 5G networks. Furthermore, the architecture must strictly enforce path separation using multiple bridges and policy-based routing to prevent the OS from short-circuiting the simulation. When combined with rigorous clock synchronization (Chrony/PTP) and measurement (OWAMP), this framework provides the necessary precision to validate the next generation of resilient media transport protocols.
Works cited
Relationships Between Gilbert-Elliot Burst Error Model Parameters and Error Statistics - Institute for Telecommunication Sciences, accessed February 16, 2026, https://its.ntia.gov/publications/download/TM-23-565.pdf
Generating more Realistic Packet Loss Patterns for Wireless links using Neural Networks, accessed February 16, 2026, https://journals.flvc.org/FLAIRS/article/download/133099/137620/246141
S4-260357-pCR_NetEmu_AI_Testbed.docx - 3GPP, accessed February 16, 2026, https://www.3gpp.org/FTP/Meetings_3GPP_SYNC/SA4/Inbox/Drafts/FS_6G_MED/S4-260357-pCR_NetEmu_AI_Testbed.docx
An Empirical Study of NetEm Network Emulation Functionalities - ResearchGate, accessed February 16, 2026, https://www.researchgate.net/publication/224256550_An_Empirical_Study_of_NetEm_Network_Emulation_Functionalities
How to Use tc (Traffic Control) with Docker Containers - OneUptime, accessed February 16, 2026, https://oneuptime.com/blog/post/2026-02-08-how-to-use-tc-traffic-control-with-docker-containers/view
How can I rate limit network traffic on a Docker container - Stack Overflow, accessed February 16, 2026, https://stackoverflow.com/questions/25497523/how-can-i-rate-limit-network-traffic-on-a-docker-container
How to tc filter with NETEM? - Stack Overflow, accessed February 16, 2026, https://stackoverflow.com/questions/24729545/how-to-tc-filter-with-netem
Steps To Implement 5G Beyond Networks in NS3 - Ns3 Projects, accessed February 16, 2026, https://ns3simulation.com/how-to-implement-5g-beyond-networks-in-ns3/
Simulating O-RAN 5G Systems in ns-3 - arXiv.org, accessed February 16, 2026, https://arxiv.org/pdf/2305.06906
How to Use Docker and NS-3 to Create Realistic Network Simulations, accessed February 16, 2026, https://www.sei.cmu.edu/blog/how-to-use-docker-and-ns-3-to-create-realistic-network-simulations/
5G 3GPP-like Channel Models for Outdoor Urban Microcellular and Macrocellular Environments - Qualcomm, accessed February 16, 2026, https://www.qualcomm.com/content/dam/qcomm-martech/dm-assets/documents/5g_3gpp-like_channel_models.pdf
Comparison of CORE network emulation platforms - Clemson University, accessed February 16, 2026, https://people.computing.clemson.edu/~jmarty/projects/lowLatencyNetworking/papers/Simulators-emulators/ComparisonofCoreWithOtherNetEmulators.pdf
Twenty-five open-source network emulators and simulators you can use in 2023, accessed February 16, 2026, https://brianlinkletter.com/2023/02/network-emulators-and-network-simulators-2023/
Open-Source Network Simulators, accessed February 16, 2026, https://opensourcenetworksimulators.com/open-source-network-simulators/
greenwich157/telco-5G-data-faults · Datasets at Hugging Face, accessed February 16, 2026, https://huggingface.co/datasets/greenwich157/telco-5G-data-faults
NYU WIRELESS TR 2022-001, accessed February 16, 2026, https://wireless.engineering.nyu.edu/wp-content/uploads/2022/01/NYU-WIRELESS-TR-2022-001.pdf
A Novel Millimeter-Wave Channel Simulator and Applications for 5G Wireless Communications - NYU Wireless, accessed February 16, 2026, https://wireless.engineering.nyu.edu/wp-content/uploads/2017/03/NYUSIM_ICC_2017_Revision_v1_3_Final.pdf
The Gilbert-Elliott Model for Packet Loss in Real Time Services on the Internet - TU Darmstadt, accessed February 16, 2026, https://www.kom.tu-darmstadt.de/papers/HH08_1034.pdf
tc-netem - Linux Man Pages Online, accessed February 16, 2026, https://man.he.net/man8/tc-netem
[2104.12959] Realtime Mobile Bandwidth and Handoff Predictions in 4G/5G Networks, accessed February 16, 2026, https://arxiv.org/abs/2104.12959
Handover Parameters Optimisation Techniques in 5G Networks - MDPI, accessed February 16, 2026, https://www.mdpi.com/1424-8220/21/15/5202
NYU WIRELESS TR 2022-004, accessed February 16, 2026, https://wireless.engineering.nyu.edu/wp-content/uploads/2022/11/TR2022-004.pdf
Macvlan network driver - Docker Docs, accessed February 16, 2026, https://docs.docker.com/engine/network/drivers/macvlan/
TR 138 901 - V14.3.0 - 5G; Study on channel model for frequencies from 0.5 to 100 GHz (3GPP TR 38.901 version 14.3.0 Release 14 - ETSI, accessed February 16, 2026, https://www.etsi.org/deliver/etsi_tr/138900_138999/138901/14.03.00_60/tr_138901v140300p.pdf
5G Network data - Kaggle, accessed February 16, 2026, https://www.kaggle.com/datasets/vinothkannaece/5g-network-data
CellReplay: Towards accurate record-and-replay for cellular networks - USENIX, accessed February 16, 2026, https://www.usenix.org/system/files/nsdi25-sentosa.pdf
CellReplay: Towards Accurate Record- and-replay for Cellular Network - USENIX, accessed February 16, 2026, https://www.usenix.org/system/files/nsdi25_slides-sentosa.pdf
How to Understand Docker Networking Internals (veth pairs, bridges) - OneUptime, accessed February 16, 2026, https://oneuptime.com/blog/post/2026-02-08-how-to-understand-docker-networking-internals-veth-pairs-bridges/view
Introduction to Linux interfaces for virtual networking | Red Hat Developer, accessed February 16, 2026, https://developers.redhat.com/blog/2018/10/22/introduction-to-linux-interfaces-for-virtual-networking
Use Docker (compose) With Different Network Interfaces / VLANs | by Jonas - Medium, accessed February 16, 2026, https://medium.com/@_jonas/use-docker-compose-with-different-network-interfaces-vlans-8ed83e5a3dbe
Routing packets from a specific Docker container through a specific outgoing interface, accessed February 16, 2026, https://stewartadam.io/blog/2019/04/04/routing-packets-specific-docker-container-through-specific-outgoing-interface
Using A Second NIC Exclusively For Docker Services - General, accessed February 16, 2026, https://forums.docker.com/t/using-a-second-nic-exclusively-for-docker-services/127180
Trouble limiting docker container bandwidth with tc - General, accessed February 16, 2026, https://forums.docker.com/t/trouble-limiting-docker-container-bandwidth-with-tc/17331
Using eBPF-TC to securely mangle packets in the kernel, and pass them to my secure networking application - OpenZiti Tech Blog, accessed February 16, 2026, https://blog.openziti.io/using-ebpf-tc-to-securely-mangle-packets-in-the-kernel-and-pass-them-to-my-secure-networking-application
Chaos in the network — using ToxiProxy for network chaos engineering | by Safeer CM | The Cloud Bulletin | Medium, accessed February 16, 2026, https://medium.com/cloudbulletin/chaos-in-the-network-using-toxiproxy-for-network-chaos-engineering-13fb0ae2deea
eBPF Fundamentals: What It Is, Why It Matters, and How It Changes Infrastructure | by Ibrahim Cisse | Jan, 2026 | Medium, accessed February 16, 2026, https://medium.com/@Ibraheemcisse/ebpf-fundamentals-what-it-is-why-it-matters-and-how-it-changes-infrastructure-557545986af0
Dint: Fast In-Kernel Distributed Transactions with eBPF - USENIX, accessed February 16, 2026, https://www.usenix.org/system/files/nsdi24-zhou-yang.pdf
Accurate Record-and-Replay for HTTP Abstract - Mahimahi - MIT, accessed February 16, 2026, http://mahimahi.mit.edu/mahimahi_atc.pdf
Configuration examples and accuracy - chrony, accessed February 16, 2026, https://chrony-project.org/examples.html
How to Run NTP Server in Docker - OneUptime, accessed February 16, 2026, https://oneuptime.com/blog/post/2026-02-08-how-to-run-ntp-server-in-docker/view
OWAMP (One-Way Active Measurement Protocol) - NetBeez, accessed February 16, 2026, https://netbeez.net/blog/owamp/
Can We Containerize Internet Measurements? - University of Oregon, accessed February 16, 2026, https://ix.cs.uoregon.edu/~ram/papers/ANRW-2019-a.pdf
