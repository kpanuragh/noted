# The web app.
FROM node:22-slim AS builder
WORKDIR /build

COPY package.json package-lock.json ./
# `npm ci` rather than `install`: it installs exactly the lockfile, so an image
# built today and one built next month contain the same tree.
RUN npm ci

COPY . .
# Inlined into the browser bundle at BUILD time — a runtime env var would be
# too late, because the JavaScript the browser runs is already written by then.
ARG NEXT_PUBLIC_API_BASE=http://localhost:8787
ENV NEXT_PUBLIC_API_BASE=$NEXT_PUBLIC_API_BASE
# Next's standalone output ships only the files the server actually needs —
# a few MB instead of the whole node_modules tree.
ENV NEXT_TELEMETRY_DISABLED=1
RUN npm run build

FROM node:22-slim AS runtime
WORKDIR /app
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1
RUN useradd --system --create-home --uid 10002 nextjs
USER nextjs

COPY --from=builder --chown=nextjs:nextjs /build/.next/standalone ./
COPY --from=builder --chown=nextjs:nextjs /build/.next/static ./.next/static
# `public/` is optional — this app has no static assets, and a COPY of a
# missing path fails the build rather than being skipped.

EXPOSE 3000
CMD ["node", "server.js"]
