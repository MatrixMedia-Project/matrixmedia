import { execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

const here = fileURLToPath(new URL('.', import.meta.url));

function git(args: string): string {
  return execSync(`git ${args}`, { cwd: here, stdio: ['ignore', 'pipe', 'ignore'] })
    .toString()
    .trim();
}

/**
 * The git commit this bundle is built from, shown in the sidebar footer.
 *
 * `MM_BUILD_COMMIT` wins: the Docker build context and a `git archive` export have no
 * .git. Otherwise git is asked, but only if it tracks this file (a `git archive` export
 * unpacked inside some other checkout would otherwise report that checkout's HEAD);
 * a package with uncommitted changes gets a `-dirty` suffix. Failing both: "unknown".
 */
function buildCommit(): string {
  const fromEnv = process.env.MM_BUILD_COMMIT?.trim();
  if (fromEnv) return fromEnv;
  try {
    git('ls-files --error-unmatch vite.config.ts');
    const dirty = git('status --porcelain -- .') !== '';
    return git('rev-parse HEAD') + (dirty ? '-dirty' : '');
  } catch {
    return 'unknown';
  }
}

export default defineConfig({
  plugins: [react()],
  base: '/_mm/dashboard/',
  define: {
    __MM_BUILD_COMMIT__: JSON.stringify(buildCommit()),
  },
  server: {
    port: 5174,
    proxy: {
      '/_mm/admin': {
        target: 'http://localhost:6168',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
    rollupOptions: {
      output: {
        manualChunks: {
          // Split React + router into a cacheable vendor chunk so first
          // paint doesn't ship all admin pages at once.
          'react-vendor': [
            'react',
            'react/jsx-runtime',
            'react-dom',
            'react-dom/client',
            'react-router-dom',
          ],
        },
      },
    },
  },
});
