use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let listener = TcpListener::bind("127.0.0.1:8080").expect("bind 127.0.0.1:8080");
    println!("Mock HTTP server listening on http://127.0.0.1:8080");

    for stream in listener.incoming() {
        if let Ok(mut stream) = stream {
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let mut buf = [0u8; 2048];
                if let Ok(n) = stream.read(&mut buf) {
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let (path, content) = if req.contains("GET /doc1.json") {
                        ("/doc1.json", fs::read_to_string("examples/doc1.json").unwrap())
                    } else if req.contains("GET /doc2.json") {
                        ("/doc2.json", fs::read_to_string("examples/doc2.json").unwrap())
                    } else {
                        ("/", "Not Found".to_string())
                    };

                    println!("[mock_server] START request: {path}");
                    // Brief sleep to make overlapping/concurrency easily observable
                    std::thread::sleep(std::time::Duration::from_millis(50));

                    let status = if content == "Not Found" {
                        "404 NOT FOUND"
                    } else {
                        "200 OK"
                    };

                    let response = format!(
                        "HTTP/1.1 {status}\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n\
                         {}",
                        content.len(),
                        content
                    );

                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    println!("[mock_server] FINISH request: {path} (elapsed: {:?})", start.elapsed());
                }
            });
        }
    }
}
