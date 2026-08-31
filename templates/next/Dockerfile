# Next.js production image — scaffolded by webo. Works with any Next app:
# `output: "standalone"` (the light path, ~150 MB) is used when the app
# enables it, and the full build ships otherwise.
FROM node:22-alpine AS build
WORKDIR /app
COPY package.json package-lock.json* ./
RUN npm ci
COPY . .
# NEXT_PRIVATE_STANDALONE forces the standalone build without touching the
# app's next.config; when the Next version ignores it, the full build below
# still ships (bigger image, same behaviour).
ENV NEXT_TELEMETRY_DISABLED=1 NEXT_PRIVATE_STANDALONE=true
RUN npm run build
RUN set -e; \
    mkdir -p /out; \
    if [ -d .next/standalone ]; then \
      cp -a .next/standalone/. /out/; \
      mkdir -p /out/.next; \
      cp -a .next/static /out/.next/static; \
    else \
      cp -a .next /out/.next; \
      cp -a node_modules /out/node_modules; \
      cp -a package.json /out/package.json; \
      for f in next.config.js next.config.mjs next.config.ts; do \
        if [ -f "$f" ]; then cp -a "$f" /out/; fi; \
      done; \
    fi; \
    if [ -d public ]; then cp -a public /out/public; fi

FROM node:22-alpine
WORKDIR /app
RUN addgroup -S app && adduser -S app -G app
COPY --from=build --chown=app /out ./
USER app
ENV NODE_ENV=production PORT=3000 HOSTNAME=0.0.0.0
EXPOSE 3000
# server.js only exists in a standalone build; otherwise fall back to next start
CMD ["sh", "-c", "if [ -f server.js ]; then node server.js; else npx --no-install next start -p ${PORT:-3000} -H 0.0.0.0; fi"]
