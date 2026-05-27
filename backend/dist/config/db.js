"use strict";
var __importDefault = (this && this.__importDefault) || function (mod) {
    return (mod && mod.__esModule) ? mod : { "default": mod };
};
Object.defineProperty(exports, "__esModule", { value: true });
exports.prisma = void 0;
const client_1 = require("@prisma/client");
const pg_1 = require("pg");
const adapter_pg_1 = require("@prisma/adapter-pg");
const dotenv_1 = __importDefault(require("dotenv"));
const tracing_1 = require("./tracing");
dotenv_1.default.config();
const connectionString = process.env.DATABASE_URL;
// Optimized connection pool configuration for high concurrency
// - max: 25 connections (tuned for 16GB memory baseline)
// - min: 5 idle connections for quick connection acquisition
// - idle timeout: 30s to reclaim unused connections
// - connection timeout: 5s to fail fast on unavailable database
const pool = new pg_1.Pool({
    connectionString,
    max: parseInt(process.env.DB_POOL_MAX || "25", 10),
    min: parseInt(process.env.DB_POOL_MIN || "5", 10),
    idleTimeoutMillis: parseInt(process.env.DB_POOL_IDLE_TIMEOUT || "30000", 10),
    connectionTimeoutMillis: parseInt(process.env.DB_POOL_CONNECTION_TIMEOUT || "5000", 10),
});
// Add pool event listeners for monitoring connection health
pool.on("error", (err, client) => {
    const logger = tracing_1.trace.getLogger("db-pool");
    logger.error(`Unexpected error on idle client: ${err.message}`, {
        error: err,
        state: "idle",
    });
});
pool.on("connect", () => {
    const logger = tracing_1.trace.getLogger("db-pool");
    logger.debug("New database connection established", {
        poolSize: pool.totalCount,
        idleCount: pool.idleCount,
    });
});
pool.on("remove", () => {
    const logger = tracing_1.trace.getLogger("db-pool");
    logger.debug("Database connection removed from pool", {
        poolSize: pool.totalCount,
        idleCount: pool.idleCount,
    });
});
const adapter = new adapter_pg_1.PrismaPg(pool);
const globalForPrisma = global;
// Initialize Prisma with optimized middleware for tracing and performance monitoring
exports.prisma = globalForPrisma.prisma ||
    new client_1.PrismaClient({
        adapter,
        log: process.env.NODE_ENV === "development" ? ["query", "error", "warn"] : ["error"],
    });
// Add query middleware for tracing and performance monitoring
exports.prisma.$use(async (params, next) => {
    const spanContext = tracing_1.context.active();
    const startTime = Date.now();
    const logger = tracing_1.trace.getLogger("db-query");
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
    }
    catch (error) {
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
if (process.env.NODE_ENV !== "production")
    globalForPrisma.prisma = exports.prisma;
