-- Direct contract test for the synthesizable profile-2 register slave. This
-- test drives the pinned bridge's unusual cyc/capture then sel/response shape;
-- CDC behavior belongs to the later apply-handshake test.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;
use work.rf_profile2_p.all;

entity rf_profile2_wb_registers_tb is end entity;

architecture simulation of rf_profile2_wb_registers_tb is
    constant FIRST_WORDS : phase_steps_t := (
        x"00000000", x"deadbeef", x"deadbeef", x"ffffffff"
    );
    constant SECOND_WORDS : phase_steps_t := (
        x"11111111", x"22222222", x"33333333", x"44444444"
    );
    constant MAX_WAIT_CYCLES : positive := 32;

    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal wb_address : std_logic_vector(31 downto 0) := (others => '0');
    signal wb_write_data : std_logic_vector(31 downto 0) := (others => '0');
    signal wb_read_data : std_logic_vector(31 downto 0);
    signal wb_write, wb_select, wb_strobe, wb_cycle, wb_ack : std_logic := '0';
    signal rx_active_sync : std_logic := '0';
    signal apply_valid, apply_done, apply_success : std_logic := '0';
    signal apply_words : phase_steps_t;
    signal apply_epoch : unsigned(31 downto 0);
    signal monitored_requests, monitored_acks : natural := 0;
begin
    clock <= not clock after 5 ns;

    dut : entity work.rf_profile2_wb_registers
        port map (
            clock => clock, reset => reset,
            wb_address => wb_address, wb_write_data => wb_write_data,
            wb_read_data => wb_read_data, wb_write => wb_write,
            wb_select => wb_select, wb_strobe => wb_strobe,
            wb_cycle => wb_cycle, wb_ack => wb_ack,
            rx_active_sync => rx_active_sync,
            apply_valid => apply_valid, apply_words => apply_words,
            apply_epoch => apply_epoch, apply_done => apply_done,
            apply_success => apply_success
        );

    monitor : process(clock, reset)
        variable request_open : boolean := false;
        variable previous_ack, previous_cycle : std_logic := '0';
    begin
        if reset = '1' then
            request_open := false;
            previous_ack := '0';
            previous_cycle := '0';
            monitored_requests <= 0;
            monitored_acks <= 0;
        elsif rising_edge(clock) then
            assert not (previous_ack = '1' and wb_ack = '1')
                report "register ACK exceeded one clock" severity failure;
            if wb_cycle = '1' and previous_cycle = '0' then
                assert wb_strobe = '1' and not request_open
                    report "invalid or overlapping register request" severity failure;
                request_open := true;
                monitored_requests <= monitored_requests + 1;
            end if;
            if wb_ack = '1' then
                assert request_open and wb_select = '1' and wb_cycle = '0'
                    report "register ACK lacked one delayed selected request"
                    severity failure;
                request_open := false;
                monitored_acks <= monitored_acks + 1;
            end if;
            previous_ack := wb_ack;
            previous_cycle := wb_cycle;
        end if;
    end process;

    stimulus : process
        procedure cycles(count : positive) is
        begin
            for index in 1 to count loop
                wait until falling_edge(clock);
            end loop;
        end procedure;

        procedure transaction(
            word_address : natural;
            write_request : boolean;
            write_value : std_logic_vector(31 downto 0);
            expected_read : std_logic_vector(31 downto 0);
            extra_select_cycles : natural := 0
        ) is
            variable before_requests, before_acks : natural;
            variable waited : natural := 0;
        begin
            before_requests := monitored_requests;
            before_acks := monitored_acks;
            wait until falling_edge(clock);
            wb_address <= std_logic_vector(to_unsigned(word_address, 32));
            wb_write_data <= write_value;
            if write_request then wb_write <= '1'; else wb_write <= '0'; end if;
            wb_strobe <= '1';
            wb_cycle <= '1';
            wb_select <= '0';

            wait until falling_edge(clock);
            -- Prove the slave uses its captured payload, not these live wires.
            wb_address <= x"fffffff0";
            wb_write_data <= x"a5a55a5a";
            wb_write <= not wb_write;
            wb_cycle <= '0';
            wb_select <= '1';

            loop
                wait until falling_edge(clock);
                exit when wb_ack = '1';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "register request did not terminate" severity failure;
            end loop;
            if not write_request then
                assert wb_read_data = expected_read
                    report "register read returned unexpected data" severity failure;
            end if;

            wait until falling_edge(clock);
            assert wb_ack = '0'
                report "register ACK did not clear after one clock" severity failure;
            for index in 1 to extra_select_cycles loop
                wait until falling_edge(clock);
                assert wb_ack = '0'
                    report "held select duplicated a register response" severity failure;
            end loop;
            assert monitored_requests = before_requests + 1
                and monitored_acks = before_acks + 1
                report "register request/ACK accounting differs" severity failure;
            wb_select <= '0';
            wb_strobe <= '0';
            wb_write <= '0';
            wait until falling_edge(clock);
        end procedure;

        procedure write_word(
            word_address : natural;
            value : std_logic_vector(31 downto 0);
            extra_select_cycles : natural := 0
        ) is
        begin
            transaction(word_address, true, value, (others => '0'), extra_select_cycles);
        end procedure;

        procedure read_word(
            word_address : natural;
            expected : std_logic_vector(31 downto 0)
        ) is
        begin
            transaction(word_address, false, (others => '0'), expected);
        end procedure;

        procedure complete_apply(success : std_logic) is
        begin
            wait until falling_edge(clock);
            apply_success <= success;
            apply_done <= '1';
            wait until falling_edge(clock);
            apply_done <= '0';
            apply_success <= '0';
        end procedure;

    begin
        assert P2_ID = x"52460201" and P2_CAPABILITIES = x"00001f04"
            and P2_COMMIT_MAGIC = x"434d4954" and P2_UNKNOWN_READ = x"bad0add0"
            and P2_ADDR_ID = 0 and P2_ADDR_CAPABILITIES = 1
            and P2_ADDR_SHADOW_FIRST = 4 and P2_ADDR_COMMIT = 8
            and P2_ADDR_STATUS = 9 and P2_ADDR_APPLIED_EPOCH = 10
            and P2_ADDR_OUTCOME_EPOCH = 11 and P2_ADDR_APPLIED_FIRST = 12
            report "profile-2 literal register ABI differs" severity failure;
        assert P2_REASON_NONE = x"0" and P2_REASON_BAD_MAGIC = x"1"
            and P2_REASON_INCOMPLETE = x"2" and P2_REASON_RX_ACTIVE = x"3"
            and P2_REASON_EPOCH_EXHAUSTED = x"4"
            and P2_REASON_APPLY_REJECTED = x"5"
            and p2_status('1', '1', '1', '1', x"a", "0101") = x"fa500000"
            report "profile-2 literal status ABI differs" severity failure;
        assert not p2_epoch_available(unsigned'(x"ffffffff"))
            and p2_candidate_epoch(unsigned'(x"ffffffff")) = unsigned'(x"ffffffff")
            and p2_commit_reason(P2_COMMIT_MAGIC, '0', "1111", unsigned'(x"ffffffff"))
                = P2_REASON_EPOCH_EXHAUSTED
            report "profile-2 epoch exhaustion helpers differ" severity failure;
        cycles(3);
        assert wb_ack = '0' and apply_valid = '0'
            and apply_epoch = 0 and apply_words = phase_steps_t'(others => (others => '0'))
            report "profile-2 register reset outputs differ" severity failure;
        reset <= '0';
        cycles(2);

        read_word(P2_ADDR_ID, P2_ID);
        read_word(P2_ADDR_CAPABILITIES, P2_CAPABILITIES);
        read_word(P2_ADDR_COMMIT, x"00000000");
        read_word(P2_ADDR_STATUS, p2_status('0', '0', '0', '0', P2_REASON_NONE, "0000"));
        read_word(P2_ADDR_APPLIED_EPOCH, x"00000000");
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000000");
        read_word(P2_ADDR_APPLIED_FIRST, x"00000000");
        read_word(2, P2_UNKNOWN_READ);
        read_word(16#100#, P2_UNKNOWN_READ);

        -- Read-only and unknown writes terminate without changing state.
        write_word(P2_ADDR_ID, x"ffffffff");
        write_word(P2_ADDR_STATUS, x"ffffffff");
        write_word(P2_ADDR_APPLIED_EPOCH, x"ffffffff");
        write_word(P2_ADDR_OUTCOME_EPOCH, x"ffffffff");
        write_word(P2_ADDR_APPLIED_FIRST, x"ffffffff");
        write_word(16#100#, x"ffffffff");
        read_word(P2_ADDR_ID, P2_ID);
        read_word(P2_ADDR_CAPABILITIES, P2_CAPABILITIES);
        read_word(P2_ADDR_STATUS, p2_status('0', '0', '0', '0', P2_REASON_NONE, "0000"));
        read_word(P2_ADDR_APPLIED_EPOCH, x"00000000");
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000000");
        read_word(P2_ADDR_APPLIED_FIRST, x"00000000");

        write_word(P2_ADDR_SHADOW_FIRST, std_logic_vector(FIRST_WORDS(0)), 3);
        read_word(P2_ADDR_SHADOW_FIRST, std_logic_vector(FIRST_WORDS(0)));
        read_word(P2_ADDR_STATUS, p2_status('0', '0', '0', '0', P2_REASON_NONE, "0001"));

        write_word(P2_ADDR_COMMIT, x"00000000");
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '0', '0', P2_REASON_BAD_MAGIC, "0001"));
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000001");

        rx_active_sync <= '1';
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '0', '1', P2_REASON_RX_ACTIVE, "0001"));
        rx_active_sync <= '0';
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '0', '0', P2_REASON_INCOMPLETE, "0001"));

        for channel in 1 to 3 loop
            write_word(P2_ADDR_SHADOW_FIRST + channel,
                std_logic_vector(FIRST_WORDS(channel)));
        end loop;
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '0', '0', P2_REASON_INCOMPLETE, "1111"));
        -- A stale high result must not complete the next request.
        apply_success <= '1';
        apply_done <= '1';
        cycles(2);
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        assert apply_valid = '1' and apply_epoch = 1 and apply_words = FIRST_WORDS
            report "accepted commit did not freeze the first payload" severity failure;
        cycles(2);
        assert apply_valid = '1'
            report "held-high stale apply result completed a new request"
            severity failure;
        apply_done <= '0';
        apply_success <= '0';
        cycles(2);
        read_word(P2_ADDR_STATUS,
            p2_status('1', '0', '0', '0', P2_REASON_NONE, "0000"));

        -- Shadow writes during busy stage the next bank, never the frozen one.
        write_word(P2_ADDR_SHADOW_FIRST, std_logic_vector(SECOND_WORDS(0)));
        assert apply_words = FIRST_WORDS and apply_epoch = 1
            report "busy shadow write changed the frozen request" severity failure;
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        read_word(P2_ADDR_STATUS,
            p2_status('1', '0', '0', '0', P2_REASON_NONE, "0001"));
        write_word(P2_ADDR_COMMIT, x"00000000");
        read_word(P2_ADDR_STATUS,
            p2_status('1', '0', '0', '0', P2_REASON_NONE, "0001"));
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000001");
        assert apply_words = FIRST_WORDS and apply_epoch = 1
            report "busy commit changed the frozen request" severity failure;

        complete_apply('1');
        assert apply_valid = '0' report "successful apply stayed busy" severity failure;
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '1', '0', P2_REASON_NONE, "0001"));
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000001");
        read_word(P2_ADDR_APPLIED_EPOCH, x"00000001");
        for channel in 0 to 3 loop
            read_word(P2_ADDR_APPLIED_FIRST + channel,
                std_logic_vector(FIRST_WORDS(channel)));
        end loop;

        for channel in 1 to 3 loop
            write_word(P2_ADDR_SHADOW_FIRST + channel,
                std_logic_vector(SECOND_WORDS(channel)));
        end loop;
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        assert apply_valid = '1' and apply_epoch = 2 and apply_words = SECOND_WORDS
            report "second commit payload differs" severity failure;
        complete_apply('0');
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '0', '0', P2_REASON_APPLY_REJECTED, "0000"));
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000002");
        read_word(P2_ADDR_APPLIED_EPOCH, x"00000001");
        read_word(P2_ADDR_APPLIED_FIRST, std_logic_vector(FIRST_WORDS(0)));

        -- A rejected apply consumed the accepted bank; four fresh writes are required.
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '0', '0', P2_REASON_INCOMPLETE, "0000"));
        for channel in 0 to 3 loop
            write_word(P2_ADDR_SHADOW_FIRST + channel,
                std_logic_vector(SECOND_WORDS(channel)));
        end loop;
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        assert apply_valid = '1' and apply_epoch = 2
            report "retry did not preserve the next epoch" severity failure;

        -- Coherent reset cancels a pending apply and all register state.
        reset <= '1';
        cycles(2);
        assert wb_ack = '0' and apply_valid = '0' and apply_epoch = 0
            and apply_words = phase_steps_t'(others => (others => '0'))
            report "busy reset left profile-2 state" severity failure;
        reset <= '0';
        cycles(2);
        complete_apply('1');
        read_word(P2_ADDR_STATUS, p2_status('0', '0', '0', '0', P2_REASON_NONE, "0000"));

        -- Reset after capture but before select must not emit a ghost response.
        wait until falling_edge(clock);
        wb_address <= std_logic_vector(to_unsigned(P2_ADDR_SHADOW_FIRST, 32));
        wb_write_data <= x"cafef00d";
        wb_write <= '1'; wb_strobe <= '1'; wb_cycle <= '1'; wb_select <= '0';
        wait until falling_edge(clock);
        wb_cycle <= '0'; wb_strobe <= '0'; reset <= '1';
        cycles(2);
        assert wb_ack = '0' report "reset request produced a ghost ACK" severity failure;
        reset <= '0';
        cycles(2);
        read_word(P2_ADDR_SHADOW_FIRST, x"00000000");
        read_word(P2_ADDR_STATUS, p2_status('0', '0', '0', '0', P2_REASON_NONE, "0000"));
        assert monitored_requests = 2 and monitored_acks = 2
            report "post-reset request/ACK totals differ" severity failure;

        report "profile-2 Wishbone register contract passed" severity note;
        finish;
    end process;

    watchdog : process
    begin
        wait for 30 us;
        assert false report "profile-2 register bench timed out" severity failure;
    end process;
end architecture;
