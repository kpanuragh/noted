import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // Needed by the Docker image: `standalone` emits a self-contained server
  // bundle with only the files it actually imports, so the runtime image ships
  // a few MB instead of the whole node_modules tree. Harmless for `next dev`.
  output: "standalone",
};

export default nextConfig;
