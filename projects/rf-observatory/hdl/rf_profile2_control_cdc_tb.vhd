-- Direct, black-box contract test for the profile-2 control/RX-domain bridge.
-- The two clocks are intentionally unrelated. This simulation checks digital
-- event and reset semantics; it cannot model metastability or establish fitted
-- timing for the bundled payload path.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;

entity rf_profile2_control_cdc_tb is
    generic (
        -- Integer picoseconds keep the clock matrix overridable on GHDL
        -- backends that cannot override a generic of physical type time.
        G_CONTROL_HALF_PERIOD_PS : positive := 5000;
        G_RX_HALF_PERIOD_PS : positive := 6500;
        G_RX_PHASE_PS : natural := 1300;
        G_MAX_WAIT_CYCLES : positive := 64
    );
end entity;

architecture simulation of rf_profile2_control_cdc_tb is
    constant CONTROL_HALF_PERIOD : time := G_CONTROL_HALF_PERIOD_PS * 1 ps;
    constant RX_HALF_PERIOD : time := G_RX_HALF_PERIOD_PS * 1 ps;
    constant RX_PHASE : time := G_RX_PHASE_PS * 1 ps;
    constant ZERO_WORDS : phase_steps_t := (others => (others => '0'));
    constant WORDS_A : phase_steps_t := (
        x"00000000", x"ffffffff", x"aaaaaaaa", x"55555555"
    );
    constant WORDS_B : phase_steps_t := (
        x"01234567", x"89abcdef", x"deadbeef", x"80000001"
    );
    constant WORDS_C : phase_steps_t := (
        x"13579bdf", x"2468ace0", x"0f0ff0f0", x"f0f00f0f"
    );
    constant WORDS_D : phase_steps_t := (
        x"11111111", x"22222222", x"44444444", x"88888888"
    );
    constant EPOCH_A : unsigned(31 downto 0) := x"00000001";
    constant EPOCH_B : unsigned(31 downto 0) := x"00000002";
    constant EPOCH_C : unsigned(31 downto 0) := x"00000003";
    constant EPOCH_D : unsigned(31 downto 0) := x"01020304";
    constant HELD_VALID_CYCLES : positive := 3;
    constant NO_RELAUNCH_CYCLES : positive := 16;

    signal control_clock : std_logic := '0';
    signal control_reset : std_logic := '1';
    signal apply_valid : std_logic := '0';
    signal apply_words : phase_steps_t := ZERO_WORDS;
    signal apply_epoch : unsigned(31 downto 0) := (others => '0');
    signal apply_done, apply_success, rx_active_sync : std_logic;

    signal rx_clock : std_logic := '0';
    signal rx_reset : std_logic := '1';
    signal rx_enable : std_logic := '0';
    signal start_pulse, start_authorized, active_valid : std_logic;
    signal active_words : phase_steps_t;
    signal active_epoch : unsigned(31 downto 0);

    signal monitored_results : natural := 0;
    signal monitored_starts : natural := 0;
begin
    control_clock <= not control_clock after CONTROL_HALF_PERIOD;

    rx_clock_driver : process
    begin
        wait for RX_PHASE;
        loop
            wait for RX_HALF_PERIOD;
            rx_clock <= not rx_clock;
        end loop;
    end process;

    dut : entity work.rf_profile2_control_cdc
        port map (
            control_clock => control_clock,
            control_reset => control_reset,
            apply_valid => apply_valid,
            apply_words => apply_words,
            apply_epoch => apply_epoch,
            apply_done => apply_done,
            apply_success => apply_success,
            rx_active_sync => rx_active_sync,
            rx_clock => rx_clock,
            rx_reset => rx_reset,
            rx_enable => rx_enable,
            start_pulse => start_pulse,
            start_authorized => start_authorized,
            active_valid => active_valid,
            active_words => active_words,
            active_epoch => active_epoch
        );

    -- Check the source-side level contract independently of the stimulus
    -- procedures. In particular, the payload must not move while valid is held
    -- through the returned one-cycle result.
    control_monitor : process(control_clock, control_reset)
        variable previous_done : std_logic := '0';
        variable request_open : boolean := false;
        variable saved_words : phase_steps_t := ZERO_WORDS;
        variable saved_epoch : unsigned(31 downto 0) := (others => '0');
    begin
        if control_reset = '1' then
            previous_done := '0';
            request_open := false;
        elsif rising_edge(control_clock) then
            assert not (previous_done = '1' and apply_done = '1')
                report "apply_done exceeded one control clock" severity failure;

            if apply_valid = '1' then
                if not request_open then
                    request_open := true;
                    saved_words := apply_words;
                    saved_epoch := apply_epoch;
                else
                    assert apply_words = saved_words and apply_epoch = saved_epoch
                        report "source payload changed while apply_valid was held"
                        severity failure;
                end if;
            else
                request_open := false;
            end if;

            if apply_done = '1' then
                assert request_open and apply_valid = '1'
                    report "apply result had no outstanding level request"
                    severity failure;
                monitored_results <= monitored_results + 1;
            end if;
            previous_done := apply_done;
        end if;
    end process;

    -- Observe registered RX outputs one edge after the DUT produces them. Every
    -- RX-enable rise must create exactly one start pulse. Invalid starts clear
    -- the complete active tuple; active data may otherwise change only at a
    -- start boundary.
    rx_monitor : process(rx_clock, rx_reset)
        variable previous_enable : std_logic := '0';
        variable expect_start : boolean := false;
        variable previous_start : std_logic := '0';
        variable previous_active_valid : std_logic := '0';
        variable previous_active_words : phase_steps_t := ZERO_WORDS;
        variable previous_active_epoch : unsigned(31 downto 0) := (others => '0');
    begin
        if rx_reset = '1' then
            previous_enable := '0';
            expect_start := false;
            previous_start := '0';
            previous_active_valid := '0';
            previous_active_words := ZERO_WORDS;
            previous_active_epoch := (others => '0');
        elsif rising_edge(rx_clock) then
            if expect_start then
                assert start_pulse = '1'
                    report "RX-enable rise did not produce a start pulse"
                    severity failure;
            else
                assert start_pulse = '0'
                    report "start pulse occurred without an RX-enable rise"
                    severity failure;
            end if;
            assert not (previous_start = '1' and start_pulse = '1')
                report "start_pulse exceeded one RX clock" severity failure;
            assert start_authorized = '0' or start_pulse = '1'
                report "start_authorized was asserted outside start_pulse"
                severity failure;

            if start_pulse = '1' then
                monitored_starts <= monitored_starts + 1;
                if start_authorized = '1' then
                    assert active_valid = '1'
                        report "authorized start did not publish an active tuple"
                        severity failure;
                else
                    assert active_valid = '0' and active_words = ZERO_WORDS
                        and active_epoch = 0
                        report "invalid start did not clear the active tuple"
                        severity failure;
                end if;
            elsif previous_enable = '0' then
                assert active_valid = '0' and active_words = ZERO_WORDS
                    and active_epoch = 0
                    report "stopped RX retained an active tuple"
                    severity failure;
            else
                assert active_valid = previous_active_valid
                    and active_words = previous_active_words
                    and active_epoch = previous_active_epoch
                    report "active tuple changed outside a start boundary"
                    severity failure;
            end if;

            expect_start := previous_enable = '0' and rx_enable = '1';
            previous_enable := rx_enable;
            previous_start := start_pulse;
            previous_active_valid := active_valid;
            previous_active_words := active_words;
            previous_active_epoch := active_epoch;
        end if;
    end process;

    stimulus : process
        variable expected_results : natural := 0;
        variable expected_starts : natural := 0;
        variable before_results, before_starts : natural;

        procedure control_cycles(count : positive) is
        begin
            for index in 1 to count loop
                wait until falling_edge(control_clock);
            end loop;
        end procedure;

        procedure rx_cycles(count : positive) is
        begin
            for index in 1 to count loop
                wait until falling_edge(rx_clock);
            end loop;
        end procedure;

        procedure expect_no_result(count : positive) is
        begin
            for index in 1 to count loop
                wait until falling_edge(control_clock);
                assert apply_done = '0'
                    report "unexpected apply result" severity failure;
            end loop;
        end procedure;

        procedure expect_no_start(count : positive) is
        begin
            for index in 1 to count loop
                wait until falling_edge(rx_clock);
                assert start_pulse = '0'
                    report "unexpected or repeated start pulse" severity failure;
            end loop;
        end procedure;

        procedure wait_rx_active(
            expected : std_logic;
            description : string
        ) is
            variable waited : natural := 0;
        begin
            loop
                wait until falling_edge(control_clock);
                exit when rx_active_sync = expected;
                waited := waited + 1;
                assert waited < G_MAX_WAIT_CYCLES
                    report description severity failure;
            end loop;
        end procedure;

        procedure begin_apply(
            words : phase_steps_t;
            epoch : unsigned(31 downto 0)
        ) is
        begin
            assert apply_valid = '0' and apply_done = '0'
                report "test attempted to overlap apply requests" severity failure;
            wait until falling_edge(control_clock);
            apply_words <= words;
            apply_epoch <= epoch;
            apply_valid <= '1';
        end procedure;

        procedure advance_to_rx_capture is
        begin
            -- The first two control edges capture the producer payload and then
            -- assert request_level. The pinned synchronizer presents that level
            -- after three RX edges; on the fourth, RX_IDLE advances to
            -- RX_CAPTURE. The following RX edge therefore makes the apply
            -- decision. Counting from the source assertion keeps this fixture
            -- deterministic for every unrelated-clock matrix.
            wait until rising_edge(control_clock);
            wait until rising_edge(control_clock);
            for index in 1 to 4 loop
                wait until rising_edge(rx_clock);
            end loop;
        end procedure;

        procedure finish_apply(
            expected_success : std_logic;
            initial_result_count : natural
        ) is
            variable waited : natural := 0;
        begin
            loop
                wait until falling_edge(control_clock);
                exit when apply_done = '1';
                waited := waited + 1;
                assert waited < G_MAX_WAIT_CYCLES
                    report "apply request did not return a bounded result"
                    severity failure;
            end loop;
            assert apply_success = expected_success
                report "apply result success flag differed" severity failure;

            -- Model the register bank: valid is still high when the result is
            -- sampled and may remain high for additional clocks. This must not
            -- launch a second request before valid is observed low.
            for index in 1 to HELD_VALID_CYCLES loop
                wait until falling_edge(control_clock);
                assert apply_done = '0'
                    report "held apply_valid duplicated the result"
                    severity failure;
            end loop;
            apply_valid <= '0';
            wait until falling_edge(control_clock);
            for index in 1 to NO_RELAUNCH_CYCLES loop
                wait until falling_edge(control_clock);
                assert apply_done = '0'
                    report "request relaunched before apply_valid re-armed"
                    severity failure;
            end loop;
            expected_results := expected_results + 1;
            assert monitored_results = initial_result_count + 1
                and monitored_results = expected_results
                report "apply request/result count differed" severity failure;
        end procedure;

        procedure apply_transaction(
            words : phase_steps_t;
            epoch : unsigned(31 downto 0);
            expected_success : std_logic
        ) is
            variable initial_result_count : natural;
        begin
            initial_result_count := monitored_results;
            begin_apply(words, epoch);
            finish_apply(expected_success, initial_result_count);
        end procedure;

        procedure expect_start_event(
            expected_authorized : std_logic;
            expected_words : phase_steps_t;
            expected_epoch : unsigned(31 downto 0)
        ) is
            variable waited : natural := 0;
            variable initial_start_count : natural;
        begin
            initial_start_count := monitored_starts;
            loop
                wait until falling_edge(rx_clock);
                exit when start_pulse = '1';
                waited := waited + 1;
                assert waited < G_MAX_WAIT_CYCLES
                    report "RX-enable rise did not produce a bounded start"
                    severity failure;
            end loop;
            assert start_authorized = expected_authorized
                report "start authorization differed" severity failure;
            if expected_authorized = '1' then
                assert active_valid = '1' and active_words = expected_words
                    and active_epoch = expected_epoch
                    report "authorized start published the wrong atomic payload"
                    severity failure;
            else
                assert active_valid = '0' and active_words = ZERO_WORDS
                    and active_epoch = 0
                    report "invalid start payload was not zero" severity failure;
            end if;
            wait until falling_edge(rx_clock);
            assert start_pulse = '0' and start_authorized = '0'
                report "start event did not clear after one RX clock"
                severity failure;
            expected_starts := expected_starts + 1;
            assert monitored_starts = initial_start_count + 1
                and monitored_starts = expected_starts
                report "RX-enable/start count differed" severity failure;
        end procedure;

        procedure assert_common_reset(
            control_first : boolean;
            skew_cycles : positive
        ) is
        begin
            if control_first then
                wait until falling_edge(control_clock);
                control_reset <= '1';
                apply_valid <= '0';
                rx_cycles(skew_cycles);
                rx_reset <= '1';
            else
                wait until falling_edge(rx_clock);
                rx_reset <= '1';
                control_cycles(skew_cycles);
                control_reset <= '1';
                apply_valid <= '0';
            end if;
            control_cycles(1);
            rx_cycles(1);
            assert apply_done = '0' and apply_success = '0'
                and rx_active_sync = '0' and start_pulse = '0'
                and start_authorized = '0' and active_valid = '0'
                and active_words = ZERO_WORDS and active_epoch = 0
                report "common reset did not clear CDC/start state"
                severity failure;
        end procedure;

        procedure release_common_reset(
            control_first : boolean;
            skew_cycles : positive
        ) is
        begin
            if control_first then
                wait until falling_edge(control_clock);
                control_reset <= '0';
                rx_cycles(skew_cycles);
                rx_reset <= '0';
            else
                wait until falling_edge(rx_clock);
                rx_reset <= '0';
                control_cycles(skew_cycles);
                control_reset <= '0';
            end if;
        end procedure;

    begin
        control_cycles(3);
        rx_cycles(3);
        assert apply_done = '0' and apply_success = '0'
            and rx_active_sync = '0' and start_pulse = '0'
            and start_authorized = '0' and active_valid = '0'
            and active_words = ZERO_WORDS and active_epoch = 0
            report "startup reset outputs differed" severity failure;

        -- Release the common image reset with bounded domain skew. The four-phase
        -- level protocol does not support an indefinitely isolated domain reset.
        release_common_reset(true, 2);
        control_cycles(5);
        rx_cycles(5);
        assert monitored_results = 0 and monitored_starts = 0
            report "reset release created a ghost event" severity failure;

        -- Every enable rise is observable, even without a token. Holding enable
        -- high must not repeat the invalid start, and active must converge back
        -- to the control domain within a bounded number of clocks.
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        expect_start_event('0', ZERO_WORDS, (others => '0'));
        wait_rx_active('1', "RX-active assertion did not synchronize");
        expect_no_start(6);
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_rx_active('0', "RX-active deassertion did not synchronize");

        -- Two stopped-RX successes arm only one token. The second complete
        -- payload supersedes the first, so the next start must atomically use B.
        apply_transaction(WORDS_A, EPOCH_A, '1');
        assert active_valid = '0' and active_words = ZERO_WORDS and active_epoch = 0
            report "apply changed active state before a start" severity failure;
        apply_transaction(WORDS_B, EPOCH_B, '1');
        assert active_valid = '0' and active_words = ZERO_WORDS and active_epoch = 0
            report "superseding apply changed active state before a start"
            severity failure;

        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        expect_start_event('1', WORDS_B, EPOCH_B);
        wait_rx_active('1', "valid start did not synchronize RX-active");
        expect_no_start(6);
        assert active_valid = '1' and active_words = WORDS_B
            and active_epoch = EPOCH_B
            report "active payload was not stable during the RX epoch"
            severity failure;
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_rx_active('0', "RX stop did not synchronize");

        -- Stop does not re-arm. The next rise is an invalid start and must clear
        -- the old active tuple rather than reusing the last applied bank.
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        expect_start_event('0', ZERO_WORDS, (others => '0'));
        wait_rx_active('1', "invalid restart did not synchronize RX-active");
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_rx_active('0', "invalid restart stop did not synchronize");

        -- Put the destination in RX_CAPTURE, then raise RX-enable for the exact
        -- RX edge that decides the apply. Start wins that same-edge collision,
        -- the start is invalid, and the apply result is rejection. The rejected
        -- request must not leave a hidden token.
        before_results := monitored_results;
        begin_apply(WORDS_C, EPOCH_C);
        advance_to_rx_capture;
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        expect_start_event('0', ZERO_WORDS, (others => '0'));
        finish_apply('0', before_results);
        wait_rx_active('1', "RX-active race did not synchronize");
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_rx_active('0', "RX-active race stop did not synchronize");
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        expect_start_event('0', ZERO_WORDS, (others => '0'));
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_rx_active('0', "post-rejection stop did not synchronize");

        -- Reset a request before it can return. Both domain-reset assertion and
        -- release orders are exercised; the cancelled request has no outcome or
        -- token, and a fresh post-reset transaction still completes exactly once.
        before_results := monitored_results;
        begin_apply(WORDS_D, EPOCH_D);
        rx_cycles(1);
        assert_common_reset(false, 2);
        release_common_reset(true, 2);
        control_cycles(6);
        rx_cycles(6);
        assert monitored_results = before_results
            report "reset-cancelled request returned a stale result"
            severity failure;
        assert active_valid = '0' and active_words = ZERO_WORDS and active_epoch = 0
            report "reset-cancelled request left active state" severity failure;

        -- Advance a second request through RX_CAPTURE so that the RX domain has
        -- raised its successful response and armed its token, then reset control
        -- before that response can traverse the return synchronizer. RX reset is
        -- deliberately delayed within the supported common-reset skew. Neither
        -- the in-flight result nor its token may reappear after release.
        before_results := monitored_results;
        before_starts := monitored_starts;
        begin_apply(WORDS_C, EPOCH_C);
        advance_to_rx_capture;
        wait until rising_edge(rx_clock);
        assert_common_reset(true, 2);
        release_common_reset(false, 2);
        control_cycles(8);
        rx_cycles(8);
        assert monitored_results = before_results
            report "late reset returned a stale in-flight result"
            severity failure;
        assert monitored_starts = before_starts and active_valid = '0'
            and active_words = ZERO_WORDS and active_epoch = 0
            report "late reset exposed stale start or active state"
            severity failure;

        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        expect_start_event('0', ZERO_WORDS, (others => '0'));
        wait_rx_active('1', "late-reset token check did not synchronize RX-active");
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_rx_active('0', "late-reset token check did not stop RX");

        -- A complete apply/start after the late cut proves that both handshake
        -- phases recovered rather than merely suppressing all later traffic.
        apply_transaction(WORDS_C, EPOCH_C, '1');
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        expect_start_event('1', WORDS_C, EPOCH_C);
        wait_rx_active('1', "late-reset recovery start did not synchronize RX-active");
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_rx_active('0', "late-reset recovery stop did not synchronize");

        apply_transaction(WORDS_D, EPOCH_D, '1');

        -- The successful request above armed a token. A common reset clears it;
        -- a later start is therefore invalid even though the applied tuple had
        -- previously returned success.
        assert_common_reset(true, 2);
        release_common_reset(false, 2);
        control_cycles(5);
        rx_cycles(5);
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        expect_start_event('0', ZERO_WORDS, (others => '0'));
        wait_rx_active('1', "post-reset RX-active did not synchronize");

        -- Keep RX enable high across another skewed common reset. Reset initializes
        -- the edge detector low, so release must yield exactly one invalid start,
        -- never an authorized reuse of the old configuration.
        assert_common_reset(false, 2);
        before_starts := monitored_starts;
        release_common_reset(true, 2);
        control_cycles(6);
        rx_cycles(6);
        expected_starts := expected_starts + 1;
        assert monitored_starts = before_starts + 1
            and monitored_starts = expected_starts
            report "held-high reset release did not yield exactly one start"
            severity failure;
        assert active_valid = '0' and active_words = ZERO_WORDS and active_epoch = 0
            report "held-high reset release reused an active payload"
            severity failure;
        wait_rx_active('1', "held-high reset release did not synchronize RX-active");
        expect_no_start(6);
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_rx_active('0', "final RX-active deassertion did not synchronize");

        expect_no_result(6);
        expect_no_start(6);
        assert monitored_results = expected_results
            and monitored_starts = expected_starts
            report "final CDC event totals differed" severity failure;
        report "profile-2 unrelated-clock control CDC contract passed"
            severity note;
        finish;
    end process;

    watchdog : process
    begin
        wait for 200 us;
        assert false report "profile-2 control CDC bench timed out"
            severity failure;
    end process;
end architecture;
