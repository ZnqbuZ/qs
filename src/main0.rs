use std::time::Instant;
use tokio::sync::mpsc;

// 配置测试参数
const TOTAL_MESSAGES: usize = 10_000_000; // 发送 1000 万条消息
const PAYLOAD_SIZE: usize = 1024;         // 每条消息 1KB
const CHANNEL_CAPACITY: usize = 100_000;  // 通道容量 (影响背压频率)

#[tokio::main]
async fn main() {
    println!("Preparing to send {} messages of size {} bytes...", TOTAL_MESSAGES, PAYLOAD_SIZE);
    println!("Channel capacity: {}", CHANNEL_CAPACITY);

    // 创建 mpsc 通道
    // payload 使用 Vec<u8> 模拟真实数据
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

    // 预先分配好一个 Payload，发送时 clone (或者为了测试纯通道开销，甚至可以不 clone 数据只传 Arc)
    // 为了模拟真实拷贝开销，我们这里让每条消息都是一个新的 Vec
    let sample_payload = vec![0u8; PAYLOAD_SIZE];

    let start = Instant::now();

    // --- Sender Task ---
    let sender_handle = tokio::spawn(async move {
        for _ in 0..TOTAL_MESSAGES {
            // 这里我们为了测试“最大吞吐”，尽量减少分配开销，直接 clone
            // 如果想测试纯粹的 channel 调度开销，可以传 Unit () 或 usize
            let msg = sample_payload.clone();
            if let Err(_) = tx.send(msg).await {
                break;
            }
        }
    });

    // --- Receiver Task ---
    let receiver_handle = tokio::spawn(async move {
        let mut count = 0;
        let mut bytes = 0;

        while let Some(msg) = rx.recv().await {
            count += 1;
            bytes += msg.len();
            if count == TOTAL_MESSAGES {
                break;
            }
        }
        (count, bytes)
    });

    // 等待接收完成
    let (received_count, received_bytes) = receiver_handle.await.unwrap();
    // 确保发送也完成了
    sender_handle.await.unwrap();

    let duration = start.elapsed();
    let seconds = duration.as_secs_f64();

    println!("\n--- Test Results ---");
    println!("Time elapsed: {:.4} seconds", seconds);
    println!("Messages received: {}", received_count);

    let msgs_per_sec = received_count as f64 / seconds;
    let mb_per_sec = (received_bytes as f64 / 1024.0 / 1024.0) / seconds;

    println!("Throughput (Msg/s): {:.2} million/s", msgs_per_sec / 1_000_000.0);
    println!("Bandwidth  (MB/s): {:.2} MB/s", mb_per_sec);
    // 如果是 Gbps
    println!("Bandwidth  (Gbps): {:.2} Gbps", mb_per_sec * 8.0 / 1024.0);
}