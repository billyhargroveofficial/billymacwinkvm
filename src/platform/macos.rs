use tracing::{info, warn};

const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

pub fn prepare_low_latency_thread(label: &'static str) {
    let result = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if result == 0 {
        info!(label, "raised macOS thread QoS to user-interactive");
    } else {
        warn!(label, result, "failed to raise macOS thread QoS");
    }
}

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}
