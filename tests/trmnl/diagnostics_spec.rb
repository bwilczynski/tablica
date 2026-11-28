# Run from trmnl-firmware v1.8.19's spec/ with its simulator harness loaded.
# TABLICA_BIN points to the Tablica executable built from this checkout.
RSpec.describe "Tablica diagnostics", env: "TRMNL_7inch5_OG_DIY_Kit" do
  [1, 2].each do |depth|
    it "returns to the same #{depth}-bit dashboard on double-click and timer wake" do
      Dir.mktmpdir("tablica-sim") do |dir|
        port = TCPServer.open("127.0.0.1", 0) { |socket| socket.addr[1] }
        # Port 80 is a separate image origin in the 2-bit run. The simulator
        # forwards it to the same Tablica instance.
        image_port = depth == 2 ? 80 : port
        url = "http://10.0.2.2:#{port}"
        config = File.join(dir, "config.toml")
        File.write(config, <<~TOML)
          listen = "127.0.0.1:#{port}"
          base_url = "http://10.0.2.2:#{image_port}"
          timezone = "UTC"
          data_dir = "#{dir}"
          [[device]]
          name = "kit"
          mac = "#{Builds::TEST_MAC}"
          api_key_env = "KIT_TEST_KEY"
          dashboard = "homelab"
          bit_depth = #{depth}
          diagnostics_button = true
          [family]
          switch_at = "16:00"
          calendar_days = 7
          [family.calendar]
          ics_url_env = "KIT_TEST_CALENDAR"
          [family.weather]
          latitude = 0
          longitude = 0
        TOML
        log = File.join(dir, "server.log")
        pid = Process.spawn({ "KIT_TEST_KEY" => "sim-test-key", "KIT_TEST_CALENDAR" => "http://127.0.0.1:1",
                              "HTTPS_PROXY" => "http://127.0.0.1:1", "RUST_LOG" => "info" },
                            ENV.fetch("TABLICA_BIN"), "serve", "--config", config, out: log, err: [:child, :out])
        begin
          eventually("Tablica did not start", within: 10) do
            raise File.read(log) if File.read(log).include?("Error:")
            begin
              TCPSocket.open("127.0.0.1", port, &:close)
              true
            rescue Errno::ECONNREFUSED
              false
            end
          end
          fresh_device(host_ports: { image_port => port }) do |s|
            s.portal_connect("TRMNL-Sim", "password", server: url)
            s.wait_for_console(%r{GET http://.*/images/homelab-})
            normal = s.wait_for_deep_sleep(display_idle: true)
            interval = normal["wake_at_s"] - normal["sim_time_s"]
            image_for = lambda do |prefix|
              path = File.read(log).scan(%r{GET (/images/#{prefix}-[^ ]+\.png) 200}).last.fetch(0)
              Net::HTTP.get(URI("http://127.0.0.1:#{port}#{path}"))
            end
            dashboard = image_for.call("homelab")
            [:double_click, :wake].each do |back|
              s.double_click(ms: 180, gap_ms: 150)
              s.wait_for_console(%r{GET http://.*/images/diag1-})
              asleep = s.wait_for_deep_sleep(display_idle: true)
              expect(asleep["wake_at_s"] - asleep["sim_time_s"]).to be_within(5).of(120)
              expect(s).to show_image(image_for.call("diag1"), tolerance: 64, max_ratio: 0)

              if back == :double_click
                s.double_click(ms: 180, gap_ms: 150)
              else
                s.wake
              end
              s.wait_for_console(%r{GET http://.*/api/display})
              asleep = s.wait_for_deep_sleep(display_idle: true)
              expect(asleep["wake_at_s"] - asleep["sim_time_s"]).to be_within(5).of(interval)
              expect(s).to show_image(dashboard, tolerance: 64, max_ratio: 0)
            end
            lines = File.readlines(log)
            expect(lines.grep(%r{GET /api/setup 200}).size).to eq(1)
            downloads = lines.grep(%r{GET /images/diag1-.*200}).grep(/user_agent="ESP32HTTPClient"/)
            expect(downloads.size).to eq(2)
            if depth == 2
              expect(downloads.grep(/device="kit"/)).to be_empty
            else
              expect(downloads.grep(/device="kit"/).size).to eq(2)
            end
          end
        ensure
          Process.kill("TERM", pid)
          Process.wait(pid)
        end
      end
    end
  end
end
