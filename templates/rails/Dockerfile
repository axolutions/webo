# Rails production image — scaffolded by webo, on the Ruby this app asks for. Two stages: gems + assets are
# built once, the runtime carries only what the app needs.
FROM ruby:{{ruby}}-slim AS build
WORKDIR /app
RUN apt-get update -qq && apt-get install -y --no-install-recommends \
    build-essential git libyaml-dev pkg-config nodejs \
    libsqlite3-dev libpq-dev && \
    rm -rf /var/lib/apt/lists/*
COPY Gemfile Gemfile.lock ./
RUN bundle config set --local deployment true && \
    bundle config set --local without "development test" && \
    bundle install --jobs 4
COPY . .
RUN SECRET_KEY_BASE_DUMMY=1 RAILS_ENV=production bundle exec rails assets:precompile || true

FROM ruby:{{ruby}}-slim
WORKDIR /app
RUN apt-get update -qq && apt-get install -y --no-install-recommends \
    libsqlite3-0 libpq5 curl && rm -rf /var/lib/apt/lists/* && \
    useradd -m rails
COPY --from=build /usr/local/bundle /usr/local/bundle
COPY --from=build --chown=rails /app /app
# Boot script: Rails 7.1 refuses to start in production without a
# SECRET_KEY_BASE, and a fresh database needs its schema. Both are handled
# here so a first deploy just works; set SECRET_KEY_BASE yourself to keep
# sessions across a volume reset. printf (not a heredoc) so any builder works.
RUN printf '%s\n' \
  '#!/bin/sh' \
  'set -e' \
  'if [ -z "$SECRET_KEY_BASE" ]; then' \
  '  KEYFILE=/app/storage/secret_key_base' \
  '  mkdir -p /app/storage' \
  '  [ -f "$KEYFILE" ] || head -c 48 /dev/urandom | od -An -tx1 | tr -d " \n" > "$KEYFILE"' \
  '  SECRET_KEY_BASE=$(cat "$KEYFILE"); export SECRET_KEY_BASE' \
  'fi' \
  'if [ -n "$DATABASE_URL" ]; then' \
  '  bundle exec rails db:prepare || echo "webo: db:prepare failed, starting anyway"' \
  'fi' \
  'exec "$@"' \
  > /usr/local/bin/webo-entry && chmod +x /usr/local/bin/webo-entry

USER rails
ENV RAILS_ENV=production RAILS_LOG_TO_STDOUT=1 RAILS_SERVE_STATIC_FILES=1 PORT=3000
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/webo-entry"]
CMD ["bundle", "exec", "rails", "server", "-b", "0.0.0.0"]
