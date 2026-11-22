use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Lock-free atomic cancellation token shared across worker, gateway, and branch tasks.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
    reason: Arc<Mutex<Option<String>>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self, reason: impl Into<String>) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            *self.reason.lock() = Some(reason.into());
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn reason(&self) -> Option<String> {
        self.reason.lock().clone()
    }
}

/// Event sink strictly guarded by a `CancelToken` — guarantees zero late-event leakage after abort.
pub struct GuardedRunSink {
    token: CancelToken,
    inner: Mutex<SinkState>,
}

struct SinkState {
    accepted: Vec<serde_json::Value>,
    rejected_late_count: usize,
}

impl GuardedRunSink {
    pub fn new(token: CancelToken) -> Self {
        Self {
            token,
            inner: Mutex::new(SinkState {
                accepted: Vec::new(),
                rejected_late_count: 0,
            }),
        }
    }

    /// Emit an event; returns `false` and increments `rejected_late_count` if token is cancelled.
    pub fn emit(&self, event: serde_json::Value) -> bool {
        let mut g = self.inner.lock();
        if self.token.is_cancelled() {
            g.rejected_late_count += 1;
            return false;
        }
        g.accepted.push(event);
        true
    }

    pub fn accepted_count(&self) -> usize {
        self.inner.lock().accepted.len()
    }

    pub fn rejected_late_count(&self) -> usize {
        self.inner.lock().rejected_late_count
    }

    pub fn accepted_events(&self) -> Vec<serde_json::Value> {
        self.inner.lock().accepted.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyName {
    Fifo,
    PriorityAging,
    WarmEnvAffinity,
    DualCacheTtlAware,
}

impl PolicyName {
    pub fn as_str(&self) -> &'static str {
        match self {
            PolicyName::Fifo => "fifo",
            PolicyName::PriorityAging => "priority_aging",
            PolicyName::WarmEnvAffinity => "warm_env_affinity",
            PolicyName::DualCacheTtlAware => "dual_cache_ttl_aware",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPriority {
    Interactive,
    Background,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub job_id: String,
    pub session_id: String,
    pub tenant_id: String,
    pub priority: JobPriority,
    pub arrival_s: f64,
    pub required_rev: usize,
    pub prefix_tokens: usize,
    pub last_cache_hit_s: Option<f64>,
    pub cache_ttl_s: f64,
    pub base_ttft_hit_s: f64,
    pub base_ttft_miss_s: f64,
    pub decode_s: f64,
    pub tool_s: f64,
    pub workspace_rebuild_s: f64,
    pub workspace_inc_sync_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutcome {
    pub job_id: String,
    pub session_id: String,
    pub priority: JobPriority,
    pub queue_wait_s: f64,
    pub prep_env_s: f64,
    pub llm_s: f64,
    pub tool_s: f64,
    pub jct_s: f64,
    pub cache_hit: bool,
    pub effective_input_cost_units: f64,
    pub warm_env_reused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForegroundMetrics {
    pub count: usize,
    pub mean_queue_wait_s: f64,
    pub p50_jct_s: f64,
    pub p95_jct_s: f64,
    pub p99_jct_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundMetrics {
    pub count: usize,
    pub mean_queue_wait_s: f64,
    pub p95_queue_wait_s: f64,
    pub max_queue_wait_s: f64,
    pub starved_jobs_over_45s: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptCacheMetrics {
    pub eligible_count: usize,
    pub overall_hit_rate: f64,
    pub expiring_hot_count: usize,
    pub expiring_hot_hit_rate: f64,
    pub total_effective_input_cost_units: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerPolicyReport {
    pub policy: String,
    pub total_jobs: usize,
    pub throughput_jobs_per_min: f64,
    pub foreground: ForegroundMetrics,
    pub background: BackgroundMetrics,
    pub prompt_cache: PromptCacheMetrics,
    pub warm_env_reuse_rate: f64,
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn uniform_01(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) / ((1u64 << 53) as f64)
    }

    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.uniform_01()
    }

    fn randint(&mut self, lo: usize, hi_inclusive: usize) -> usize {
        let span = (hi_inclusive - lo + 1) as u64;
        lo + (self.next_u64() % span) as usize
    }

    fn expovariate(&mut self, lambda: f64) -> f64 {
        let u = (1.0 - self.uniform_01()).max(1e-12);
        -u.ln() / lambda
    }
}

fn round_n(v: f64, decimals: u32) -> f64 {
    let f = 10_f64.powi(decimals as i32);
    (v * f).round() / f
}

fn percentile(vals: &[f64], q: f64) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    let mut s = vals.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let raw = (q * s.len() as f64).ceil() as isize - 1;
    let idx = raw.clamp(0, (s.len() - 1) as isize) as usize;
    s[idx]
}

/// Generate a multi-session workload mixing interactive hot-expiring sessions and background bursts.
pub fn generate_rate_limited_workload(
    seed: u64,
    n_jobs: usize,
    overload: bool,
    real_ttft_hit_s: f64,
    real_decode_s: f64,
) -> Vec<ScheduledJob> {
    let mut rng = SplitMix64::new(seed);
    let mut jobs = Vec::with_capacity(n_jobs);
    let mut t = 0.0_f64;
    let mean_gap = if overload { 0.42 } else { 0.78 };

    for i in 0..n_jobs {
        if matches!(i % 12, 1..=4) {
            t += rng.uniform(0.05, 0.18);
            jobs.push(ScheduledJob {
                job_id: format!("bg-{i:03}"),
                session_id: format!("bg-sess-{}", i % 10),
                tenant_id: format!("tenant-{}", i % 3),
                priority: JobPriority::Background,
                arrival_s: round_n(t, 4),
                required_rev: i / 6,
                prefix_tokens: rng.randint(3_000, 12_000),
                last_cache_hit_s: None,
                cache_ttl_s: 300.0,
                base_ttft_hit_s: real_ttft_hit_s,
                base_ttft_miss_s: real_ttft_hit_s + 0.6,
                decode_s: real_decode_s * rng.uniform(0.8, 1.3),
                tool_s: rng.uniform(0.08, 0.25),
                workspace_rebuild_s: 0.30,
                workspace_inc_sync_s: 0.002,
            });
            continue;
        }

        t += rng.expovariate(1.0 / mean_gap);
        let is_hot_expiring = i % 3 == 0;
        let (prefix_tokens, last_hit) = if is_hot_expiring {
            let elapsed = rng.uniform(278.0, 296.5);
            (rng.randint(80_000, 160_000), Some(round_n(t - elapsed, 4)))
        } else {
            let elapsed = rng.uniform(20.0, 120.0);
            let lh = if i % 5 != 0 {
                Some(round_n(t - elapsed, 4))
            } else {
                None
            };
            (rng.randint(25_000, 70_000), lh)
        };

        let miss_penalty = (prefix_tokens as f64 / 100_000.0) * 2.8;
        jobs.push(ScheduledJob {
            job_id: format!("fg-{i:03}"),
            session_id: format!("fg-sess-{}", i % 14),
            tenant_id: format!("tenant-{}", i % 3),
            priority: JobPriority::Interactive,
            arrival_s: round_n(t, 4),
            required_rev: i / 6,
            prefix_tokens,
            last_cache_hit_s: last_hit,
            cache_ttl_s: 300.0,
            base_ttft_hit_s: real_ttft_hit_s,
            base_ttft_miss_s: real_ttft_hit_s + miss_penalty,
            decode_s: real_decode_s * rng.uniform(0.75, 1.25),
            tool_s: rng.uniform(0.05, 0.20),
            workspace_rebuild_s: 0.30,
            workspace_inc_sync_s: 0.002,
        });
    }

    jobs.sort_by(|a, b| {
        a.arrival_s
            .partial_cmp(&b.arrival_s)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    jobs
}

struct WorkerSlot {
    free_at: f64,
    rev: Option<usize>,
    tenant_id: Option<String>,
}

/// Event-driven discrete-time simulation of worker pool + provider concurrency slot scheduling.
pub fn simulate_scheduler_policy(
    jobs: &[ScheduledJob],
    policy: PolicyName,
    n_workers: usize,
    aging_starvation_cap_s: f64,
    ttl_rescue_window_s: f64,
    min_rescue_prefix_tokens: usize,
) -> SchedulerPolicyReport {
    let mut workers: Vec<WorkerSlot> = (0..n_workers)
        .map(|_| WorkerSlot {
            free_at: 0.0,
            rev: None,
            tenant_id: None,
        })
        .collect();
    let mut pending: Vec<ScheduledJob> = Vec::new();
    let mut remaining_idx = 0usize;
    let mut outcomes: Vec<JobOutcome> = Vec::with_capacity(jobs.len());

    while remaining_idx < jobs.len() || !pending.is_empty() {
        let w_idx = workers
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.free_at
                    .partial_cmp(&b.free_at)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap();
        let mut now_s = workers[w_idx].free_at;

        while remaining_idx < jobs.len() && jobs[remaining_idx].arrival_s <= now_s {
            pending.push(jobs[remaining_idx].clone());
            remaining_idx += 1;
        }

        if pending.is_empty() && remaining_idx < jobs.len() {
            let next_t = jobs[remaining_idx].arrival_s;
            for w in &mut workers {
                if w.free_at < next_t {
                    w.free_at = next_t;
                }
            }
            now_s = next_t;
            while remaining_idx < jobs.len() && jobs[remaining_idx].arrival_s <= now_s {
                pending.push(jobs[remaining_idx].clone());
                remaining_idx += 1;
            }
        }

        let chosen_idx = if policy == PolicyName::Fifo {
            0
        } else {
            let w_tenant = workers[w_idx].tenant_id.as_deref();
            let w_rev = workers[w_idx].rev;
            pending
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| {
                    let sa = score_job(
                        a,
                        now_s,
                        w_tenant,
                        w_rev,
                        policy,
                        aging_starvation_cap_s,
                        ttl_rescue_window_s,
                        min_rescue_prefix_tokens,
                    );
                    let sb = score_job(
                        b,
                        now_s,
                        w_tenant,
                        w_rev,
                        policy,
                        aging_starvation_cap_s,
                        ttl_rescue_window_s,
                        min_rescue_prefix_tokens,
                    );
                    sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(idx, _)| idx)
                .unwrap()
        };

        let job = pending.remove(chosen_idx);
        let queue_wait = (now_s - job.arrival_s).max(0.0);

        let warm_reused = workers[w_idx].tenant_id.as_deref() == Some(job.tenant_id.as_str())
            && workers[w_idx].rev.is_some();
        let prep_s = if policy == PolicyName::Fifo {
            if warm_reused && workers[w_idx].rev == Some(job.required_rev) {
                0.001
            } else {
                job.workspace_rebuild_s
            }
        } else if warm_reused {
            job.workspace_inc_sync_s
        } else {
            job.workspace_rebuild_s
        };

        let llm_start_s = now_s + prep_s;
        let cache_hit = match job.last_cache_hit_s {
            Some(lh) => (llm_start_s - lh) <= job.cache_ttl_s,
            None => false,
        };
        let ttft_s = if cache_hit {
            job.base_ttft_hit_s
        } else {
            job.base_ttft_miss_s
        };
        let llm_s = ttft_s + job.decode_s;
        let cost_units = (job.prefix_tokens as f64 * if cache_hit { 0.10 } else { 1.15 }) / 1000.0;
        let finish_s = llm_start_s + llm_s + job.tool_s;

        workers[w_idx].free_at = finish_s;
        workers[w_idx].rev = Some(job.required_rev);
        workers[w_idx].tenant_id = Some(job.tenant_id.clone());

        outcomes.push(JobOutcome {
            job_id: job.job_id,
            session_id: job.session_id,
            priority: job.priority,
            queue_wait_s: round_n(queue_wait, 4),
            prep_env_s: round_n(prep_s, 4),
            llm_s: round_n(llm_s, 4),
            tool_s: round_n(job.tool_s, 4),
            jct_s: round_n(finish_s - job.arrival_s, 4),
            cache_hit,
            effective_input_cost_units: round_n(cost_units, 2),
            warm_env_reused: warm_reused,
        });
    }

    let job_map: HashMap<&str, &ScheduledJob> =
        jobs.iter().map(|j| (j.job_id.as_str(), j)).collect();
    let fg: Vec<&JobOutcome> = outcomes
        .iter()
        .filter(|o| o.priority == JobPriority::Interactive)
        .collect();
    let bg: Vec<&JobOutcome> = outcomes
        .iter()
        .filter(|o| o.priority == JobPriority::Background)
        .collect();

    let eligible: Vec<&JobOutcome> = outcomes
        .iter()
        .filter(|o| job_map[o.job_id.as_str()].last_cache_hit_s.is_some())
        .collect();
    let hot_expiring: Vec<&JobOutcome> = eligible
        .iter()
        .copied()
        .filter(|o| {
            let j = job_map[o.job_id.as_str()];
            (j.arrival_s - j.last_cache_hit_s.unwrap_or(0.0)) >= 270.0
        })
        .collect();

    let min_arr = jobs
        .iter()
        .map(|j| j.arrival_s)
        .fold(f64::INFINITY, f64::min);
    let max_end = outcomes
        .iter()
        .map(|o| job_map[o.job_id.as_str()].arrival_s + o.jct_s)
        .fold(0.0_f64, f64::max);
    let makespan_s = (max_end - min_arr).max(0.001);
    let total_cost: f64 = outcomes.iter().map(|o| o.effective_input_cost_units).sum();

    let fg_jcts: Vec<f64> = fg.iter().map(|o| o.jct_s).collect();
    let bg_waits: Vec<f64> = bg.iter().map(|o| o.queue_wait_s).collect();

    SchedulerPolicyReport {
        policy: policy.as_str().to_string(),
        total_jobs: outcomes.len(),
        throughput_jobs_per_min: round_n(outcomes.len() as f64 / (makespan_s / 60.0), 2),
        foreground: ForegroundMetrics {
            count: fg.len(),
            mean_queue_wait_s: round_n(
                fg.iter().map(|o| o.queue_wait_s).sum::<f64>() / fg.len().max(1) as f64,
                3,
            ),
            p50_jct_s: round_n(percentile(&fg_jcts, 0.50), 3),
            p95_jct_s: round_n(percentile(&fg_jcts, 0.95), 3),
            p99_jct_s: round_n(percentile(&fg_jcts, 0.99), 3),
        },
        background: BackgroundMetrics {
            count: bg.len(),
            mean_queue_wait_s: round_n(
                bg.iter().map(|o| o.queue_wait_s).sum::<f64>() / bg.len().max(1) as f64,
                3,
            ),
            p95_queue_wait_s: round_n(percentile(&bg_waits, 0.95), 3),
            max_queue_wait_s: round_n(bg_waits.iter().copied().fold(0.0_f64, f64::max), 3),
            starved_jobs_over_45s: bg.iter().filter(|o| o.queue_wait_s > 45.0).count(),
        },
        prompt_cache: PromptCacheMetrics {
            eligible_count: eligible.len(),
            overall_hit_rate: round_n(
                eligible.iter().filter(|o| o.cache_hit).count() as f64
                    / eligible.len().max(1) as f64,
                4,
            ),
            expiring_hot_count: hot_expiring.len(),
            expiring_hot_hit_rate: round_n(
                hot_expiring.iter().filter(|o| o.cache_hit).count() as f64
                    / hot_expiring.len().max(1) as f64,
                4,
            ),
            total_effective_input_cost_units: round_n(total_cost, 1),
        },
        warm_env_reuse_rate: round_n(
            outcomes.iter().filter(|o| o.warm_env_reused).count() as f64
                / outcomes.len().max(1) as f64,
            4,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn score_job(
    job: &ScheduledJob,
    now_s: f64,
    worker_tenant: Option<&str>,
    worker_rev: Option<usize>,
    policy: PolicyName,
    aging_starvation_cap_s: f64,
    ttl_rescue_window_s: f64,
    min_rescue_prefix_tokens: usize,
) -> f64 {
    let wait = (now_s - job.arrival_s).max(0.0);
    let mut score = match job.priority {
        JobPriority::Interactive => 100.0 + wait * 2.0,
        JobPriority::Background => 10.0 + (wait / aging_starvation_cap_s.max(1.0)) * 95.0,
    };

    if matches!(
        policy,
        PolicyName::WarmEnvAffinity | PolicyName::DualCacheTtlAware
    ) {
        if worker_tenant == Some(job.tenant_id.as_str()) && worker_rev.is_some() {
            score += 18.0;
        }
        if worker_rev == Some(job.required_rev) {
            score += 8.0;
        }
    }

    if policy == PolicyName::DualCacheTtlAware {
        if let Some(lh) = job.last_cache_hit_s {
            let rem_ttl = (lh + job.cache_ttl_s) - now_s;
            if rem_ttl > 0.0
                && rem_ttl <= ttl_rescue_window_s
                && job.prefix_tokens >= min_rescue_prefix_tokens
            {
                let urgency = (ttl_rescue_window_s - rem_ttl) / ttl_rescue_window_s;
                let value = (job.prefix_tokens as f64 / 80_000.0).min(2.0);
                score += 180.0 + 120.0 * urgency * value;
            } else if rem_ttl <= 0.0 {
                score -= 5.0;
            }
        }
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancel_token_and_guarded_sink() {
        let tok = CancelToken::new();
        let sink = GuardedRunSink::new(tok.clone());
        assert!(sink.emit(serde_json::json!({"event": "chunk_1"})));
        tok.cancel("sibling_won");
        assert!(tok.is_cancelled());
        assert_eq!(tok.reason().as_deref(), Some("sibling_won"));
        assert!(!sink.emit(serde_json::json!({"event": "late_chunk_2"})));
        assert!(!sink.emit(serde_json::json!({"event": "late_chunk_3"})));
        assert_eq!(sink.accepted_count(), 1);
        assert_eq!(sink.rejected_late_count(), 2);
    }

    #[test]
    fn test_dual_scheduler_rescues_hot_cache_and_cuts_cost() {
        let jobs = generate_rate_limited_workload(2026, 120, false, 2.937, 0.602);
        let fifo = simulate_scheduler_policy(&jobs, PolicyName::Fifo, 3, 12.0, 35.0, 40_000);
        let dual =
            simulate_scheduler_policy(&jobs, PolicyName::DualCacheTtlAware, 3, 12.0, 35.0, 40_000);

        assert!(
            dual.prompt_cache.expiring_hot_hit_rate > fifo.prompt_cache.expiring_hot_hit_rate,
            "dual={} vs fifo={}",
            dual.prompt_cache.expiring_hot_hit_rate,
            fifo.prompt_cache.expiring_hot_hit_rate
        );
        assert!(
            dual.prompt_cache.total_effective_input_cost_units
                < fifo.prompt_cache.total_effective_input_cost_units
        );
        assert!(dual.foreground.p95_jct_s < fifo.foreground.p95_jct_s);
    }
}
