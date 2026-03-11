import { loadConfig } from './config.js';
import { createLogger } from './logger.js';
import { TaskRouter } from './router.js';

async function main(): Promise<void> {
  const config = loadConfig();
  const log = createLogger(config.logLevel);

  log.info({ version: '0.1.0' }, 'QFC Task Router starting');

  if (!config.routerPrivateKey) {
    log.warn('ROUTER_PRIVATE_KEY not set — task claiming will fail');
  }

  const router = new TaskRouter(config, log);

  // Graceful shutdown
  const shutdown = async (signal: string) => {
    log.info({ signal }, 'Received shutdown signal');
    await router.stop();
    process.exit(0);
  };

  process.on('SIGINT', () => void shutdown('SIGINT'));
  process.on('SIGTERM', () => void shutdown('SIGTERM'));

  await router.start();
}

main().catch((err) => {
  console.error('Fatal error:', err);
  process.exit(1);
});
