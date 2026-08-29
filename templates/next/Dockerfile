# Next.js production image — scaffolded by webo. Requires `output: "standalone"`
# in next.config; the runtime carries only the standalone server.
FROM node:22-alpine AS build
WORKDIR /app
COPY package.json package-lock.json* ./
RUN npm ci
COPY . .
ENV NEXT_TELEMETRY_DISABLED=1
RUN npm run build

FROM node:22-alpine
WORKDIR /app
RUN addgroup -S app && adduser -S app -G app
COPY --from=build --chown=app /app/.next/standalone ./
COPY --from=build --chown=app /app/.next/static ./.next/static
COPY --from=build --chown=app /app/public ./public
USER app
ENV NODE_ENV=production PORT=3000 HOSTNAME=0.0.0.0
EXPOSE 3000
CMD ["node", "server.js"]
