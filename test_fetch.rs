use memstroy_tg;

#[tokio::main]
async fn main() {
    env_logger::init();
    
    println!("Testing Telegram fetch...");
    
    match memstroy_tg::fetch_all("mellstroy", 2).await {
        Ok(posts) => {
            println!("Successfully fetched {} posts", posts.len());
            for (i, post) in posts.iter().take(3).enumerate() {
                println!("Post {}: id={}, images={}, desc_len={}", 
                    i+1, post.id, post.images.len(), post.clean_description().len());
            }
        }
        Err(e) => {
            eprintln!("Error fetching posts: {}", e);
        }
    }
}
