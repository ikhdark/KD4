@{
    # Isolated serving presets. No KD4 model or provider is selected by this file.
    Common = @(
        '--host', '127.0.0.1',
        '--alias', 'qwen-coder',
        '--n-gpu-layers', 'all',
        '--kv-unified',
        '--flash-attn', 'on',
        '--batch-size', '512',
        '--ubatch-size', '128',
        '--cont-batching',
        '--no-ui'
    )
    Profiles = @{
        SharedFanout = @(
            '--model', 'target-q8_0.gguf',
            '--ctx-size', '24576', '--parallel', '4',
            '--cache-ram', '0'
        )
        SourceCopy = @(
            '--model', 'target-q8_0.gguf',
            '--ctx-size', '45056', '--parallel', '4',
            '--cache-ram', '0', '--temp', '0',
            '--spec-type', 'ngram-simple',
            '--spec-ngram-simple-size-n', '6',
            '--spec-ngram-simple-size-m', '16'
        )
        CompactWeights = @(
            '--model', 'target-q4_k_m.gguf',
            '--ctx-size', '24576', '--parallel', '4',
            '--cache-ram', '0'
        )
        KvCapacity = @(
            '--model', 'target-q8_0.gguf',
            '--ctx-size', '14848', '--parallel', '2',
            '--cache-ram', '0',
            '--cache-type-k', 'q8_0', '--cache-type-v', 'q8_0'
        )
        HostRestore = @(
            '--model', 'target-q8_0.gguf',
            '--ctx-size', '24576', '--parallel', '1',
            '--cache-ram', '384',
            '--slot-prompt-similarity', '1'
        )
    }
}
