# frozen_string_literal: true

BoringBuilder.configure do |config|
  config.artifact.directory("/app/dist", at: "/app")

  config.pipeline do |pipeline|
    # The project's mise.toml overrides this fallback.
    app = pipeline.mise(tools: { node: "22" }, workdir: "/app")

    app = pipeline.run(
      app,
      %w[npm ci],
      cache: "npm-downloads",
      at: "/root/.npm/_cacache",
      workdir: "/app",
      name: "Install dependencies"
    )

    app = pipeline.exec(app, ["test", "!", "-e", "/root/.npm/_cacache"], name: "Verify clean cache mount")
    pipeline.exec(app, %w[npm run build], name: "Build application")
  end
end
