Advanced Architectures for Cellular Bonding in Real-Time Video: A Comprehensive Technical Report
1. The Physics of Heterogeneity in Multi-Link Aggregation
1.1 The Operational Environment of Cellular Bonding
The modern landscape of live video transmission—ranging from mobile newsgathering (ENG) to teleoperation of autonomous vehicles—relies increasingly on the aggregation of commodity cellular links. The premise is deceptively simple: bond 2 to 6 disparate modems (4G LTE, 5G Sub-6, 5G mmWave) to form a single, high-bandwidth logical pipe. However, the stochastic nature of wireless channels transforms this integration into a complex control theory problem. Unlike wired links, where capacity is deterministic and latency is dominated by propagation delay, cellular links exhibit "bufferbloat," varying spectral efficiency, and abrupt capacity collapses due to mobility and handover events.
The core challenge in bonding is not merely summing capacities () but managing the variance in packet delivery times, known as Packet Delay Variation (PDV) or jitter. When a packet scheduler naïvely stripes data across a low-latency 5G link (15ms RTT) and a congested 4G link (150ms RTT), the receiver buffer must hold the "fast" packets hostage until the "slow" packets arrive to ensure in-order delivery to the video decoder. This phenomenon, Head-of-Line (HoL) blocking, is the primary adversary of low-latency streaming. Recent empirical studies 1 indicate that in highly heterogeneous environments, the effective goodput of a bonded connection can paradoxically fall below that of the single fastest link—a condition known as the "multipath penalty."
1.2 Radio Resource Management (RRM) and Link Dynamics
To build a scheduler that transcends basic Round Robin, one must understand the physical layer metrics that drive link performance. The modem does not behave as a black box; it exposes Radio Resource Management (RRM) data that correlates with future capacity.
RSRP (Reference Signal Received Power): Often mistaken for "signal quality," RSRP measures signal strength (range). A high RSRP (-70 dBm) does not guarantee throughput if the tower is congested. However, the trend (first derivative) of RSRP is a critical leading indicator of mobility-induced handovers.2
RSRQ (Reference Signal Received Quality): This metric normalizes RSRP against the Received Signal Strength Indicator (RSSI), providing a measure of spectral efficiency. High RSRP paired with low RSRQ (-15 dB) indicates high interference or heavy tower loading—a "phantom link" that looks strong but delivers poor throughput.3
SINR (Signal to Interference plus Noise Ratio): The most volatile and valuable metric. SINR dictates the Modulation and Coding Scheme (MCS). A drop in SINR forces the eNodeB to shift from 256-QAM to 16-QAM or QPSK, instantly slashing link capacity by 75% or more. Schedulers that ignore SINR variance cannot anticipate these "capacity cliffs".5
CQI (Channel Quality Indicator): Derived from SINR, this integer (0-15) is what the User Equipment (UE) reports to the tower to request a specific data rate. By monitoring the CQI reported by the modem, the scheduler gains a window into the negotiated capacity of the link before the TCP stack perceives a slowdown.3
Table 1 illustrates the divergence between these metrics and their impact on scheduling decisions.
Metric
Update Frequency
Correlation to Throughput
Correlation to Handoff
Scheduler Utility
RSRP
Medium (100ms)
Low
High (Coverage)
Predicting blackouts/coverage loss.
RSRQ
Medium (100ms)
Medium
Medium (Interference)
Identifying congested sectors.
SINR
High (10-20ms)
High (MCS Determination)
Low
Predicting immediate capacity shifts.
CQI
High (2-80ms)
Very High
N/A
Estimating transport block sizes.

1.3 The Failure of Legacy Algorithms
Standard Deficit Weighted Round Robin (DWRR) schedulers operate on the assumption that link capacity changes slowly. They assign weights () based on smoothed throughput averages. In a cellular environment, where a vehicle passing behind a building can induce a 20dB drop in SINR in milliseconds, DWRR reacts too slowly. It continues to push packets into a degrading queue, causing a burst of late packets that stall the video decoder. The research focus from 2022 to 2026 has therefore pivoted toward proactive, delay-aware, and learning-based schedulers that treat the link as a stochastic process rather than a static pipe.
2. Deterministic Scheduling Paradigms: Beyond MinRTT
While heuristic algorithms lack the adaptability of AI, recent deterministic designs have introduced sophisticated logic to handle heterogeneity without the computational overhead of neural networks. These algorithms form the baseline for any advanced scheduler.
2.1 BLEST: Blocking Estimation-Based Scheduler
The Blocking Estimation-based MPTCP Scheduler (BLEST) represents a fundamental shift from latency minimization to blocking minimization. Adopted into the Linux kernel, BLEST posits that it is often optimal to keep a slow path idle rather than risk blocking the receive window.1
2.1.1 Mechanism of Action
BLEST continuously evaluates the risk of HoL blocking by comparing the transmission potential of the fastest available subflow against the current subflow. It utilizes a specific decision inequality. Let  be the path with the lowest RTT and  be the path currently being considered.
The scheduler estimates the amount of data  that  could transmit during the time it takes  to deliver a single segment (i.e., during ).

If transmitting a packet on  would fill the receive window () such that  becomes blocked (i.e., unable to transmit  bytes) upon its next availability, BLEST executes a Wait action. It explicitly stalls the application write to , waiting for  to become available.7
2.1.2 Algorithmic Logic
The pseudocode below illustrates the core decision loop of BLEST, incorporating the dynamic penalty factor  used to tune aggressiveness.

Python


# BLEST Scheduler Pseudocode
# Inputs:
#   subflows: List of available links
#   R_window: Receiver's advertised window
#   inflight: Total bytes currently inflight
def schedule_packet_blest(subflows, R_window, inflight):
    # Identify candidates
    fast_path = min(subflows, key=lambda s: s.srtt)
    available_paths = [s for s in subflows if s.cwnd > s.unacked]
    
    if not available_paths:
        return WAIT
    
    # If the fast path is open, always use it
    if fast_path in available_paths:
        return fast_path
        
    # Decision for slow path candidate
    candidate = available_paths 
    
    # Estimate capacity of fast path during slow path's RTT
    # Note: Using standard bandwidth-delay product estimation
    estimated_fast_capacity = (fast_path.bandwidth) * candidate.srtt
    
    # Calculate potential window exhaustion
    remaining_window = R_window - inflight
    
    # BLEST Condition:
    # If the fast path can carry more data than the remaining window allows,
    # and using the slow path contributes to blocking...
    if estimated_fast_capacity > remaining_window:
        # Apply correctional factor delta (learned from recent HoL events)
        if estimated_fast_capacity * candidate.delta > remaining_window:
            return WAIT # Do not use the slow path
            
    return candidate


Critique: BLEST excels in scenarios with extreme asymmetry (e.g., Starlink vs. DSL). However, its reliance on accurate bandwidth estimation can be problematic in cellular networks where "bandwidth" is highly elastic and buffer-dependent.8
2.2 ECF: Earliest Completion First
The ECF scheduler 1 moves beyond per-packet RTT to consider the completion time of the entire data queue. It models the transmission as a "race" where the finish line is the arrival of the last byte at the receiver.
2.2.1 Completion Time Modeling
For a given packet  and path , the predicted arrival time  is calculated as:

Where  is the current backlog of data on path ,  is the size of packet , and  is the estimated bandwidth. ECF assigns the packet to the path  that minimizes . This effectively "water-fills" the paths, ensuring that all paths finish transmitting their assigned queues simultaneously.
2.2.2 Impact on Video
For live video, ECF is particularly potent because video is generated in discrete units (frames or GOPs). ECF naturally aligns with the concept of "flushing" a video frame across multiple links so that the tail arrives as quickly as possible, minimizing frame reassembly delay.10
2.3 DEMS: Decoupled Multipath Scheduler
DEMS (Delay-Aware Multipath Scheduler) 11 addresses the specific issue of "tail blocking" where the last few packets of a chunk get stuck on a slow link.
2.3.1 Chunk Splitting and Reinjection
DEMS splits a data chunk (e.g., a video slice) into two sub-sequences. Path 1 transmits from the beginning (Index ); Path 2 transmits from the end (Index ). They meet in the middle.
The critical innovation is Adaptive Reinjection. If Path 1 reaches the "meeting point" before Path 2, it does not stop. It begins to retransmit (redundantly) the packets that Path 2 is currently struggling to deliver. This creates a race condition where the receiver accepts whichever copy arrives first.
Comparison to ReMP: The ReMP (Redundant Multipath) scheduler replicates all traffic.12 While this minimizes latency, it halves aggregate throughput. DEMS provides a "graceful" redundancy, only consuming extra bandwidth when one link is underperforming relative to the prediction.11
2.4 IoDS: In-order Delivery Scheduler (2025 Innovation)
Proposed in late 2025, the In-order Delivery Scheduler (IoDS) 13 is a response to the limitations of MinRTT in highly heterogeneous 5G/Wi-Fi environments.
2.4.1 The Monotonic Arrival Constraint
IoDS strictly enforces that packets are scheduled in an order that ensures their predicted arrival times are monotonic. If packet  is sent on a slow path and packet  on a fast path, IoDS inserts a scheduling delay for  or reorders the assignment to ensure .
Results: In real-world tests involving 4K video, IoDS reduced the standard deviation of download times by 71% compared to the Linux default scheduler, effectively eliminating micro-stutters caused by out-of-order buffering.14
2.5 ALCS: Adaptive Latency Compensation Scheduler (2025/2026)
Originating from research into Satellite-Terrestrial Integrated Networks (STIN), ALCS 15 offers mechanisms highly applicable to cellular bonding, particularly for predicting handovers.
2.5.1 Trajectory-Aware Prediction
In satellite networks, latency changes follow a predictable orbital trajectory. ALCS incorporates this derivative into its RTT estimator. In cellular bonding, this concept translates to mobility-aware prediction, where the scheduler infers "virtual trajectories" (e.g., moving away from the cell center) based on RSRP slope.
2.5.2 The Handover Buffer
A key feature of ALCS is the "One-RTT Buffer". When a handover (or satellite switch) is predicted, ALCS enforces a silence period equivalent to  on the degrading link. This allows in-flight packets to drain before the link breaks, preventing the need for massive retransmissions during the break-before-make transition of cellular handovers.15
3. Stochastic and Learning-Based Scheduling: The "Brain"
While deterministic algorithms rely on rigid models (), cellular links are chaotic. Bandwidth is not a constant; it is a random variable. This necessitates the use of Reinforcement Learning (RL) and Multi-Armed Bandits (MAB).
3.1 The Contextual Bandit Approach: Peekaboo (LinUCB)
Peekaboo 16 frames subflow selection as a Contextual Multi-Armed Bandit (CMAB) problem. It does not assume a static reward for each link but assumes the reward depends on the current context (state).
3.1.1 State Features (Context )
Peekaboo constructs a context vector  containing:
Smoothed RTT (SRTT): Moving average of delay.
RTT Variance (Jitter): Crucial for detecting congestion vs. propagation delay.
CWND Saturation: Ratio of inflight bytes to CWND.
Loss History: Recent ACK/NACK ratio.
Chunk Size: Size of the video frame to be sent.17
3.1.2 LinUCB Algorithm Details
Standard UCB assumes arms are independent. LinUCB assumes the expected reward  of arm  is linear with respect to the context: .
The scheduler selects the path  that maximizes:

: The learned coefficient vector for path  (relationship between metrics and throughput).
: The exploration term. If the scheduler has not tested a link in a specific context (e.g., "High SINR but High RTT"), the confidence interval width () is large, encouraging the scheduler to try that link to gather data. This allows Peekaboo to adapt to counter-intuitive scenarios, such as a high-loss link that still offers high throughput due to massive bandwidth.17
3.2 Thompson Sampling: Handling Delayed Feedback
While LinUCB is powerful, it assumes immediate feedback. In live video, ACKs can be delayed by hundreds of milliseconds. Thompson Sampling (TS) has emerged as superior to UCB in these "delayed feedback" environments.18
3.2.1 Probabilistic Selection
Instead of calculating a deterministic score, TS models the reward of each link as a probability distribution (typically a Beta distribution for Bernoulli rewards like packet success/fail).
Step 1: For each link , maintain success count  and failure count .
Step 2: Sample a value  from the distribution .
Step 3: Select link  with the highest sampled .
Why it works: If a link has few samples, the Beta distribution is wide (high variance). Sampling from it might yield a high value, triggering exploration. As data accumulates, the distribution narrows (high confidence). The randomization inherently handles the noise and delay of cellular feedback better than UCB's deterministic bound.21
3.3 Deep Reinforcement Learning (DRL): PPO and SAC
For global policy decisions (e.g., "Should I enable FEC?", "What is the global redundancy ratio?"), Deep RL is applicable.
3.3.1 MPQUIC DQN Architecture
A 2022 study 22 applied Deep Q-Networks (DQN) to MPQUIC scheduling.
State: A high-dimensional vector including buffer occupancy, per-path CWND, RTT, and encoded bitrate.
Reward Function: Designed to balance throughput and delay penalties:

Crucially, the reward is computed per chunk (episode), not per packet, to stabilize learning.
Computational Cost: Running a neural network inference for every packet (e.g., 1000 packets/sec) is prohibitive on ARM edge devices (Cortex-A53). Therefore, DRL is best used hierarchically: the DRL agent sets the parameters (weights) for a lightweight weighted round-robin scheduler every 500ms, while the lightweight scheduler handles per-packet dispatch.24
3.3.2 PPO vs. Bandits
Proximal Policy Optimization (PPO) 24 offers stability in continuous action spaces (e.g., setting precise pacing rates). However, research indicates that for the specific task of link selection, Contextual Bandits (LinUCB) or Thompson Sampling offer a superior trade-off between performance and CPU overhead (microsecond inference) compared to PPO (millisecond inference).24
4. Predictive Link Quality: Anticipating the Blackout
Reactive schedulers wait for packet loss to detect a bad link. In live video, this is too late. A robust scheduler must be predictive, utilizing physical layer precursors to anticipate failures.
4.1 Leading Indicators of Link Failure
Analysis of 4G/5G drive-test data reveals distinct signatures preceding a "blackout" (disconnection or deep fade):
RSRP Slope: A consistent negative slope in RSRP (e.g., -3 dB/s) is the strongest predictor of an A3 Handover Event (switching towers).2
SINR Variance: As a user moves to the cell edge, SINR variance increases due to inter-cell interference. High SINR variance often precedes a drop in CQI and MCS.3
RSRQ Divergence: A condition where RSRP is stable/high but RSRQ drops sharply (-15 to -20 dB) indicates "pollution"—the device can "hear" the tower clearly, but the spectrum is saturated with noise. This predicts packet loss despite strong signal bars.4
4.2 Prediction Algorithms
To implement the "Oracle" component of the scheduler, we employ lightweight time-series forecasting.
4.2.1 Kalman Filtering
The raw RSRP/SINR reported by modems is noisy. A Kalman Filter is the optimal estimator to smooth this data and extract the true trend (velocity of signal decay) with minimal CPU cost.27
State Model: We model the signal quality  (Signal Level) and  (Rate of Change).

The Kalman filter recursively updates this state based on noisy measurements . The estimated  (slope) acts as the trigger for "Pre-Handoff" states.
4.2.2 LSTM (Long Short-Term Memory)
For longer-horizon prediction (1-2 seconds), LSTMs 2 can learn temporal dependencies, such as the periodic fading of a signal as a vehicle passes regular obstacles (e.g., streetlights).
Implementation: An LSTM taking a sequence of 50 past SINR/CQI samples can predict the average capacity for the next 500ms.
Caveat: Requires training data specific to the mobility pattern (highway vs. urban canyon).
4.2.3 The "Dead Zone" Heuristic (Slope Detection)
For immediate (50-100ms) blackout avoidance, simple linear regression over a short window is highly effective.2 Algorithm: If the linear slope of RSRP over the last 500ms is  dB/s AND current RSRQ  dB:  Trigger PRE_HANDOVER mode (Drain queue, enable 100% redundancy).
5. Coded Bonding: Random Linear Network Coding (RLNC)
In cellular bonding, packet loss is rarely random; it is bursty. Traditional ARQ (retransmission) introduces  to  delays, which often exceeds the buffering budget of live video. Forward Error Correction (FEC) is required.
5.1 RLNC vs. Block Codes
Traditional FEC (Reed-Solomon) operates on fixed blocks. The encoder must wait for  packets to arrive before generating parity packets, introducing "block delay." Random Linear Network Coding (RLNC) 29 utilizes a sliding window approach. It generates repair packets that are random linear combinations of the packets currently in the window.
Rateless Property: The sender can generate an infinite stream of repair packets. The receiver can recover the window as soon as it receives any  linearly independent packets (source or repair). This removes the need to acknowledge specific missing packets; the receiver just asks for "more degrees of freedom."
5.2 Performance on ARM Hardware
Running RLNC on embedded routers (e.g., Cortex-A72/A53) requires careful optimization. Benchmarks using the Steinwurf Kodo library 29 reveal:
Throughput: A single Cortex-A72 core can sustain ~400-800 Mbps encoding/decoding throughput for small generation sizes ( to ).
Generation Size (): There is a trade-off. Large  (e.g., 1000) handles long burst losses but increases decoding complexity ( or ) and latency. For real-time video, small  () is optimal to keep latency < 50ms while protecting against typical cellular micro-bursts.29
SIMD Acceleration: Utilizing NEON instructions on ARM is mandatory to achieve these speeds. Without SIMD, throughput drops by ~4-10x.31
Systematic Coding:
To minimize CPU load, the scheduler should use Systematic RLNC.
Send  source packets unencoded (Systematic).
Send  coded repair packets immediately after.
Benefit: If no loss occurs, the receiver reads the systematic packets with zero decoding CPU cost. Decoding is only triggered when a packet is missing.32
6. Graceful Degradation: Cross-Layer Cooperation
The transport layer cannot solve bandwidth deficits via scheduling alone. When the aggregate capacity drops below the video bitrate, the system must degrade gracefully rather than stalling.
6.1 Scalable Video Coding (SVC)
AV1-SVC 33 allows the video bitstream to be structured into layers:
Base Layer (BL): Essential data (low resolution/fps).
Enhancement Layers (EL): Add detail/smoothness.
Mapping Strategy:
The scheduler effectively runs two logical queues:
High Priority (BL): Mapped to the most stable links (high RSRQ). Uses high redundancy (ReMP or 50% RLNC overhead).
Low Priority (EL): Mapped to opportunistic links (high variance). Uses low/no redundancy.
If the opportunistic links fail, the EL packets are dropped. The video quality lowers, but the stream continues uninterrupted.
6.2 Joint Transport-Codec Control (Salsify/GRACE)
Salsify 35 proposes a tighter loop. The video encoder does not run at a fixed bitrate. Instead, for every frame, it encodes a "high quality" and "low quality" version. The transport layer, knowing the exact instantaneous capacity, selects the version that fits. GRACE 36 uses a neural codec trained to handle loss. It treats lost packets as "masked pixels" and uses the neural net to inpaint the missing data. This allows the stream to remain watchable even with 20-30% packet loss, reducing the need for heavy FEC overhead.
6.3 Region of Interest (ROI)
Using lightweight CNNs (e.g., MobileNet) to detect faces or action, the encoder can tag packets as "ROI." The scheduler prioritizes ROI packets, ensuring that even under severe congestion, the subject of the video remains artifact-free while the background degrades.37
7. The "NeuroBond" Architecture: Synthesis & Pseudocode
We propose a hybrid architecture fusing Thompson Sampling for decision making, Kalman Filters for prediction, IoDS for buffer management, and RLNC for reliability.
7.1 Architecture Diagram
Input: Video Stream (AV1-SVC) + 2-6 Cellular Modems.
Layer 1 (Physical): LinkAnalyst runs Kalman Filters on RSRP/SINR at 100Hz.
Layer 2 (Decision): ThompsonScheduler selects links per-packet based on Beta distributions.
Layer 3 (Protection): KodoEncoder applies Systematic RLNC.
Layer 4 (Feedback): RateController signals safe bitrate to Encoder.
7.2 Detailed Algorithm Pseudocode
The following pseudocode demonstrates the integration of the concepts.
Component A: Predictive Link State Assessment

Python


# Runs every 10ms for each link
def assess_link_health(link_history):
    # 1. Kalman Filter to smooth noisy SINR readings 
    # State:
    smooth_sinr, sinr_trend = KalmanFilter(link_history.sinr).predict()
    
    # 2. Calculate RSRP Slope (Trend) over last 500ms window 
    # A sharp negative slope indicates moving away from tower
    rsrp_slope = LinearRegression(link_history.rsrp[-50:]).slope
    
    # 3. Determine Link State
    # PRE_HANDOFF: Slope is negative and signal is weak 
    if rsrp_slope < -2.5 and link_history.rsrq < -12:
        return LinkState.DANGER_HANDOFF_IMMINENT
    
    # CONGESTED: Signal is strong (RSRP) but Quality is low (RSRQ) 
    elif link_history.rsrp > -90 and link_history.rsrq < -15:
        return LinkState.PHANTOM_CONGESTION
        
    else:
        return LinkState.STABLE


Component B: Thompson Sampling Scheduler (Contextual)

Python


class ThompsonScheduler:
    def __init__(self, num_links):
        # Beta distribution parameters (alpha=success, beta=failure)
        # Initialized to 1 (Uniform prior)
        self.alphas = np.ones(num_links)
        self.betas = np.ones(num_links)

    def select_links(self, packet_priority, packet_size):
        # 1. Sample expected success prob from Beta distrib 
        sampled_scores = [beta.rvs(self.alphas[i], self.betas[i]) for i in range(num_links)]
        
        # 2. Contextual Adjustment 
        for i, link in enumerate(links):
            # Penalize based on Predictive Health (Component A)
            if link.state == LinkState.DANGER_HANDOFF_IMMINENT:
                sampled_scores[i] *= 0.01 # Effectively disable link
            elif link.state == LinkState.PHANTOM_CONGESTION:
                sampled_scores[i] *= 0.5

            # IoDS Logic : In-order Delivery Constraint
            # Penalize links where RTT + QueueDelay implies out-of-order arrival
            fastest_arrival = min(l.rtt + l.queue_delay for l in links)
            predicted_arrival = link.rtt + (link.bytes_in_queue + packet_size) / link.bandwidth
            
            # BLEST-style Wait Logic : 
            # If blocking is severe, score goes negative
            if predicted_arrival > fastest_arrival + MAX_JITTER_BUFFER:
                sampled_scores[i] = -1.0 

        # 3. Select Best Link
        best_link = np.argmax(sampled_scores)
        
        # 4. Adaptive Redundancy Decision (SVC aware)
        selected_links = [best_link]
        
        # If Base Layer (Critical) AND best link is not perfectly stable
        if packet_priority == BASE_LAYER and sampled_scores[best_link] < 0.95:
            # ReMP logic: Replicate on second best link
            second_best = np.argsort(sampled_scores)[-2]
            if sampled_scores[second_best] > 0.2: # Only if usable
                selected_links.append(second_best)
                
        return selected_links

    def update_feedback(self, link_index, success, latency):
        # Thompson Update Rule
        # [19]: TS handles delayed feedback robustly
        if success:
            self.alphas[link_index] += 1
        else:
            self.betas[link_index] += 1
            
        # Sliding window decay (optional) to handle non-stationarity
        if self.alphas[link_index] + self.betas[link_index] > WINDOW_SIZE:
            self.alphas[link_index] *= 0.95
            self.betas[link_index] *= 0.95


Component C: RLNC Encoding Integration

C++


// C++ Logic for Steinwurf Kodo 
void send_video_chunk(uint8_t* data, size_t size, Priority priority) {
    // 1. Configure Encoder
    // Small K for low latency 
    uint32_t symbols = (size + SYMBOL_SIZE - 1) / SYMBOL_SIZE;
    encoder.set_symbols_storage(data, size);
    
    // 2. Dynamic Redundancy Rate
    // If priority is high, add 50% overhead, else 10%
    float overhead = (priority == BASE_LAYER)? 1.5 : 1.1; 
    
    // 3. Generate & Send Systematic Symbols (Zero CPU cost)
    for (int i = 0; i < symbols; ++i) {
        auto pkt = encoder.encode_systematic_symbol(i);
        auto links = scheduler.select_links(priority, pkt.size());
        for (auto link : links) link.transmit(pkt);
    }
    
    // 4. Generate & Send Repair Symbols (Network Coding)
    int repair_count = symbols * (overhead - 1.0);
    for (int i = 0; i < repair_count; ++i) {
        auto pkt = encoder.encode_repair_symbol(); // Computation happens here
        // Send repairs on diverse links to maximize recovery probability
        auto links = scheduler.select_diverse_links(); 
        for (auto link : links) link.transmit(pkt);
    }
}


8. Conclusion
The construction of a bonded cellular scheduler for live video is a multidimensional optimization problem. The research from 2022-2026 demonstrates that maximizing aggregate throughput is insufficient; the scheduler must minimize the "tail" of the latency distribution.
By adopting BLEST-style blocking estimation 6 or IoDS 14, the scheduler prevents slow links from poisoning the stream latency. By utilizing Thompson Sampling 18, it robustly navigates the noisy, delayed-feedback environment of cellular networks better than heuristic methods. Finally, by integrating Predictive Handoff Detection 2 and RLNC 29, the system creates a "self-healing" stream that preemptively mitigates the inevitable instability of mobile networks. This architecture transforms the bonding router from a simple load balancer into an intelligent, predictive agent capable of delivering broadcast-grade reliability over best-effort consumer hardware.
Works cited
ECF: An MPTCP Path Scheduler to Manage Heterogeneous Paths | Request PDF, accessed February 16, 2026, https://www.researchgate.net/publication/319946409_ECF_An_MPTCP_Path_Scheduler_to_Manage_Heterogeneous_Paths
Machine Learning-Based Prediction of RSRP in Cellular Networks - Aaltodoc, accessed February 16, 2026, https://aaltodoc.aalto.fi/bitstreams/fbf2c9a6-f897-48e9-b352-57f008690f85/download
SINR, RSRP, RSSI AND RSRQ MEASUREMENTS IN LONG TERM EVOLUTION NETWORKS - OPUS at UTS, accessed February 16, 2026, https://opus.lib.uts.edu.au/rest/bitstreams/1e4a4206-eafe-4ee7-a9d3-13195aeef2bf/retrieve
LTE RSSI, RSRP and RSRQ Measurement - CableFree, accessed February 16, 2026, https://www.cablefree.net/wirelesstechnology/4glte/rsrp-rsrq-measurement-lte/
Comparing RSRP, CQI, and SINR measurements with predictions for coordinated and uncoordinated LTE small cell networks | Semantic Scholar, accessed February 16, 2026, https://www.semanticscholar.org/paper/Comparing-RSRP%2C-CQI%2C-and-SINR-measurements-with-for-Weitzen-Wakim/b6e00a34d3d4f2b5664a5b9f4eca11b8b58caf09
Saflo: eBPF-Based MPTCP Scheduler for Mitigating Traffic Analysis Attacks in Cellular Networks - arXiv, accessed February 16, 2026, https://arxiv.org/html/2502.04236v1
BLEST: Blocking Estimation-based MPTCP Scheduler for Heterogeneous Networks, accessed February 16, 2026, https://opendl.ifip-tc6.org/db/conf/networking/networking2016/1570234725.pdf
[PDF] BLEST: Blocking estimation-based MPTCP scheduler for heterogeneous networks, accessed February 16, 2026, https://www.semanticscholar.org/paper/BLEST%3A-Blocking-estimation-based-MPTCP-scheduler-Oliveira-Alay/db055ee0d6d5b419e1365af40ed82f71f8e0903c
ECF: An MPTCP Path Scheduler to Manage Heterogeneous Paths, accessed February 16, 2026, https://www.repository.cam.ac.uk/bitstreams/3ec47f93-4360-4630-bd4a-9e1ed23605fa/download
ECF: An MPTCP Path Scheduler to Manage Heterogeneous Paths - University of Cambridge, accessed February 16, 2026, https://www.repository.cam.ac.uk/bitstreams/9f216be1-4124-4f10-bbad-137e912cc7ff/download
Accelerating Multipath TransportThrough Balanced Subflow ..., accessed February 16, 2026, https://feng-qian.github.io/paper/dems_mobicom17.pdf
i2t/rmptcp: Redudant scheduler for MPTCP - GitHub, accessed February 16, 2026, https://github.com/i2t/rmptcp
Research Papers - bwNET2.0, accessed February 16, 2026, https://bwnet2.belwue.de/publications/research-papers/index.html
IoDS: A Novel MPTCP Scheduler for Heterogeneous Networks ..., accessed February 16, 2026, https://ieeexplore.ieee.org/document/11146366
ALCS: An Adaptive Latency Compensation Scheduler for Multipath TCP in Satellite-Terrestrial Integrated Networks - IEEE Computer Society, accessed February 16, 2026, https://www.computer.org/csdl/journal/tm/2026/01/11106945/28P9keutDQk
Peekaboo: Learning-Based Multipath Scheduling for Dynamic ..., accessed February 16, 2026, https://www.researchgate.net/publication/340033695_Peekaboo_Learning-Based_Multipath_Scheduling_for_Dynamic_Heterogeneous_Environments
Peekaboo: Learning-based Multipath Scheduling for ... - Ozgu Alay, accessed February 16, 2026, https://ozgualay.com/wp-content/uploads/2020/05/jsac_multipath_scheduler.pdf
Upper Confidence Bound vs Thompson Sampling - YouTube, accessed February 16, 2026, https://www.youtube.com/watch?v=e4f0or7x5xc
Why am I getting better performance with Thompson sampling than with UCB or $\epsilon$-greedy in a multi-armed bandit problem? - AI Stack Exchange, accessed February 16, 2026, https://ai.stackexchange.com/questions/21917/why-am-i-getting-better-performance-with-thompson-sampling-than-with-ucb-or-ep
IntelligentPooling: Practical Thompson Sampling for mHealth - PMC, accessed February 16, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC8494236/
Comparison of Video Recommendation Effects of Etc, Ucb, and Thompson Sampling Algorithms on Short-Video Platforms, accessed February 16, 2026, https://www.itm-conferences.org/articles/itmconf/pdf/2025/09/itmconf_cseit2025_04027.pdf
Reinforcement Learning Based Multipath QUIC Scheduler for Multimedia Streaming - PMC, accessed February 16, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC9460924/
Multi-path Scheduling with Deep Reinforcement Learning | Request PDF - ResearchGate, accessed February 16, 2026, https://www.researchgate.net/publication/335200840_Multi-path_Scheduling_with_Deep_Reinforcement_Learning
Multi-Path Routing Algorithm Based on Deep Reinforcement Learning for SDN - MDPI, accessed February 16, 2026, https://www.mdpi.com/2076-3417/13/22/12520
Multi-Path Routing Algorithm Based on Deep Reinforcement Learning for SDN, accessed February 16, 2026, https://www.researchgate.net/publication/375782141_Multi-Path_Routing_Algorithm_Based_on_Deep_Reinforcement_Learning_for_SDN
Machine Learning for Signal Loss and Link Quality Prediction in O-RAN — Enabled Cellular Networks | by Mahmoud Abdelaziz, PhD | Jan, 2026 | Medium, accessed February 16, 2026, https://medium.com/@mahmoudabdelaziz_67006/machine-learning-for-signal-loss-and-link-quality-prediction-in-o-ran-enabled-cellular-networks-0f81dd3f55fa
A Review of Applicable Technologies, Routing Protocols, Requirements, and Architecture for Disaster Area Networks - IEEE Xplore, accessed February 16, 2026, https://ieeexplore.ieee.org/iel8/6287639/6514899/11006058.pdf
Recurrent Neural Network Based Link Quality Prediction for Fluctuating Low Power Wireless Links - PMC, accessed February 16, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC8838954/
Kodo Throughput Benchmarks Comparison (RaptorQ) - Steinwurf, accessed February 16, 2026, https://www.steinwurf.com/blog/benchmark-kodo
Benchmark - RLNC vs RaptorQ - Steinwurf, accessed February 16, 2026, https://www.steinwurf.com/blog/benchmark-rlnc-vs-raptorq
Fulcrum: Flexible Network Coding for Heterogeneous Devices - IEEE Xplore, accessed February 16, 2026, https://ieeexplore.ieee.org/iel7/6287639/6514899/08554264.pdf
Computational costs of using RLNC and S2HNC as a function of number of... - ResearchGate, accessed February 16, 2026, https://www.researchgate.net/figure/Computational-costs-of-using-RLNC-and-S2HNC-as-a-function-of-number-of-source-packets-M_fig5_346476200
Multipath Dynamic Adaptive Streaming over HTTP Using Scalable Video Coding in Software Defined Networking - MDPI, accessed February 16, 2026, https://www.mdpi.com/2076-3417/10/21/7691
Mastering the AV1 SVC chains - Medooze - Medium, accessed February 16, 2026, https://medooze.medium.com/mastering-the-av1-svc-chains-a4b2a6a23925
Salsify: Low-Latency Network Video through Tighter ... - USENIX, accessed February 16, 2026, https://www.usenix.org/system/files/conference/nsdi18/nsdi18-fouladi.pdf
GRACE: Loss-Resilient Real-Time Video through Neural Codecs - USENIX, accessed February 16, 2026, https://www.usenix.org/system/files/nsdi24-cheng.pdf
Region-of-Interest Based Coding Scheme for Live Videos - MDPI, accessed February 16, 2026, https://www.mdpi.com/2076-3417/14/9/3823
