import { PrismaClient } from "@prisma/client";
import { Pool } from "pg";
import { PrismaPg } from "@prisma/adapter-pg";
import dotenv from "dotenv";
import { trace, context } from "./tracing";

dotenv.config();

const connectionString = process.env.DATABASE_URL;

// Optimized connection pool configuration for high concurrency
// - max: 25 connections (tuned for 16GB memory baseline)
// - min: 5 idle connections for quick connection acquisition
// - idle timeout: 30s to reclaim unused connections
// - connection timeout: 5s to fail fast on unavailable database
const pool = new Pool({
  connectionString,
  max: parseInt(process.env.DB_POOL_MAX || "25", 10),
  min: parseInt(process.env.DB_POOL_MIN || "5", 10),
  idleTimeoutMillis: parseInt(process.env.DB_POOL_IDLE_TIMEOUT || "30000", 10),
  connectionTimeoutMillis: parseInt(process.env.DB_POOL_CONNECTION_TIMEOUT || "5000", 10),
});

// Add pool event listeners for monitoring connection health
pool.on("error", (err: Error, client) => {
  const logger = trace.getLogger("db-pool");
  logger.error(`Unexpected error on idle client: ${err.message}`, {
    error: err,
    state: "idle",
  });
});

pool.on("connect", () => {
  const logger = trace.getLogger("db-pool");
  logger.debug("New database connection established", {
    poolSize: pool.totalCount,
    idleCount: pool.idleCount,
  });
});

pool.on("remove", () => {
  const logger = trace.getLogger("db-pool");
  logger.debug("Database connection removed from pool", {
    poolSize: pool.totalCount,
    idleCount: pool.idleCount,
  });
});

const adapter = new PrismaPg(pool);

const globalForPrisma = global as unknown as { prisma: PrismaClient };

// Initialize Prisma with optimized middleware for tracing and performance monitoring
export const prisma =
  globalForPrisma.prisma ||
  new PrismaClient({
    adapter,
    log: process.env.NODE_ENV === "development" ? ["query", "error", "warn"] : ["error"],
  });

// Add query middleware for tracing and performance monitoring
prisma.$use(async (params, next) => {
  const spanContext = context.active();
  const startTime = Date.now();
  const logger = trace.getLogger("db-query");

  try {
    const result = await next(params);
    const duration = Date.now() - startTime;

    // Log slow queries (> 1000ms)
    if (duration > 1000) {
      logger.warn(`Slow query detected: ${params.model}.${params.action}`, {
        duration,
        model: params.model,
        action: params.action,
        args: JSON.stringify(params.args).substring(0, 200),
      });
    }

    logger.debug(`Query completed: ${params.model}.${params.action}`, {
      duration,
      model: params.model,
      action: params.action,
    });

    return result;
  } catch (error) {
    const duration = Date.now() - startTime;
    logger.error(`Query failed: ${params.model}.${params.action}`, {
      duration,
      model: params.model,
      action: params.action,
      error: error instanceof Error ? error.message : String(error),
    });
    throw error;
  }
});

if (process.env.NODE_ENV !== "production") globalForPrisma.prisma = prisma;
