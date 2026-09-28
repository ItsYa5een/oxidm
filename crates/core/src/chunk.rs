#[derive(Debug, Clone, Copy)]
pub struct Chunk {
    pub id: usize,
    pub start: u64,
    pub end: u64,
}

impl Chunk {
    pub fn calculate_chunks(total_size: u64, num_chunks: u32) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        let chunk_size = total_size / (num_chunks as u64);

        for i in 0..num_chunks {
            let start = (i as u64) * chunk_size;
            let end = if i == num_chunks - 1 {
                total_size - 1
            } else {
                ((i as u64) + 1) * chunk_size - 1
            };

            chunks.push(Chunk {
                id: i as usize,
                start,
                end,
            });
        }

        chunks
    }
}