import { defineConfig } from 'vite'
import { viteSingleFile } from 'vite-plugin-singlefile'

const gateway = 'http://127.0.0.1:8001'

export default defineConfig({
  plugins: [viteSingleFile()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
  },
  server: {
    proxy: {
      '/v1': gateway,
      '/zed': gateway,
      '/api': gateway,
      '/health': gateway,
    },
  },
})
