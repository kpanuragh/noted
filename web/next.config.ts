import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // The Playwright e2e suite navigates via 127.0.0.1 (playwright.config.ts
  // baseURL), which Next.js 16's dev-origin protection otherwise treats as
  // cross-origin and silently blocks, breaking client hydration.
  allowedDevOrigins: ["127.0.0.1"],
};

export default nextConfig;
