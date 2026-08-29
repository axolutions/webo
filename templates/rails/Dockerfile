# Rails production image — scaffolded by webo. Two stages: gems + assets are
# built once, the runtime carries only what the app needs.
FROM ruby:3.3-slim AS build
WORKDIR /app
RUN apt-get update -qq && apt-get install -y --no-install-recommends \
    build-essential git libyaml-dev pkg-config libsqlite3-dev nodejs && \
    rm -rf /var/lib/apt/lists/*
COPY Gemfile Gemfile.lock ./
RUN bundle config set --local deployment true && \
    bundle config set --local without "development test" && \
    bundle install --jobs 4
COPY . .
RUN SECRET_KEY_BASE_DUMMY=1 RAILS_ENV=production bundle exec rails assets:precompile || true

FROM ruby:3.3-slim
WORKDIR /app
RUN apt-get update -qq && apt-get install -y --no-install-recommends \
    libsqlite3-0 curl && rm -rf /var/lib/apt/lists/* && \
    useradd -m rails
COPY --from=build /usr/local/bundle /usr/local/bundle
COPY --from=build --chown=rails /app /app
USER rails
ENV RAILS_ENV=production RAILS_LOG_TO_STDOUT=1 RAILS_SERVE_STATIC_FILES=1 PORT=3000
EXPOSE 3000
CMD ["bundle", "exec", "rails", "server", "-b", "0.0.0.0"]
