-- Integrate the exact pinned Wishbone master with the real profile-2 register
-- slave. Both bridge clocks are one clock, as planned for the top level. The
-- master receives the generated-system reset and the slave receives the source
-- system reset; reset skew is exercised explicitly.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;
use work.rf_profile2_p.all;

entity rf_profile2_wishbone_integration_tb is end entity;

architecture simulation of rf_profile2_wishbone_integration_tb is
    constant WORDS : phase_steps_t := (
        x"b3333333", x"e6666666", x"1999999a", x"4ccccccd"
    );
    constant MAX_WAIT_CYCLES : positive := 64;

    signal clock : std_logic := '0';
    signal system_reset : std_logic := '1';
    signal qsys_reset : std_logic := '1';
    signal av_address : std_logic_vector(31 downto 0) := (others => '0');
    signal av_write_data : std_logic_vector(31 downto 0) := (others => '0');
    signal av_read_data : std_logic_vector(31 downto 0);
    signal av_write, av_read : std_logic := '0';
    signal av_wait, av_read_valid, interrupt : std_logic;

    signal wb_address, wb_write_data, wb_read_data : std_logic_vector(31 downto 0);
    signal wb_write, wb_select, wb_strobe, wb_ack, wb_cycle : std_logic;
    signal apply_valid, apply_done, apply_success : std_logic := '0';
    signal apply_words : phase_steps_t;
    signal apply_epoch : unsigned(31 downto 0);
    signal monitored_requests, monitored_acks : natural := 0;
begin
    clock <= not clock after 5 ns;

    bridge : entity work.wishbone_master
        generic map (ADDR_BITS => 32, DATA_BITS => 32)
        port map (
            clock => clock, reset => qsys_reset,
            addr => av_address, din => av_write_data, dout => av_read_data,
            write => av_write, read => av_read, waitreq => av_wait,
            readack => av_read_valid, intr => interrupt,
            wb_clk_i => clock, wb_rst_i => system_reset,
            wb_adr_o => wb_address, wb_dat_o => wb_write_data,
            wb_dat_i => wb_read_data, wb_we_o => wb_write,
            wb_sel_o => wb_select, wb_stb_o => wb_strobe,
            wb_ack_i => wb_ack, wb_cyc_o => wb_cycle
        );

    registers : entity work.rf_profile2_wb_registers
        port map (
            clock => clock, reset => system_reset,
            wb_address => wb_address, wb_write_data => wb_write_data,
            wb_read_data => wb_read_data, wb_write => wb_write,
            wb_select => wb_select, wb_strobe => wb_strobe,
            wb_cycle => wb_cycle, wb_ack => wb_ack,
            rx_active_sync => '0', apply_valid => apply_valid,
            apply_words => apply_words, apply_epoch => apply_epoch,
            apply_done => apply_done, apply_success => apply_success
        );

    monitor : process(clock, system_reset, qsys_reset)
        variable request_open : boolean := false;
        variable previous_ack, previous_cycle : std_logic := '0';
    begin
        if system_reset = '1' or qsys_reset = '1' then
            request_open := false;
            previous_ack := '0';
            previous_cycle := '0';
            monitored_requests <= 0;
            monitored_acks <= 0;
        elsif rising_edge(clock) then
            assert wb_strobe = '1' and interrupt = '0'
                report "pinned bridge constant outputs differ" severity failure;
            assert not (previous_ack = '1' and wb_ack = '1')
                report "integrated ACK exceeded one clock" severity failure;
            if wb_cycle = '1' and previous_cycle = '0' then
                assert not request_open
                    report "integrated requests overlapped" severity failure;
                request_open := true;
                monitored_requests <= monitored_requests + 1;
            end if;
            if wb_ack = '1' then
                assert request_open and wb_select = '1' and wb_cycle = '0'
                    report "integrated ACK lacked a delayed request" severity failure;
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

        procedure release_reset is
        begin
            system_reset <= '0';
            cycles(2);
            qsys_reset <= '0';
            cycles(3);
            assert av_wait = '0' and av_read_valid = '0'
                and wb_cycle = '0' and wb_select = '0' and wb_ack = '0'
                and apply_valid = '0'
                report "integrated reset release was not idle" severity failure;
        end procedure;

        procedure assert_skewed_reset is
        begin
            av_read <= '0';
            av_write <= '0';
            system_reset <= '1';
            cycles(1);
            qsys_reset <= '1';
            cycles(2);
            assert av_wait = '0' and av_read_valid = '0'
                and wb_cycle = '0' and wb_select = '0' and wb_ack = '0'
                and apply_valid = '0'
                report "coherent reset left integrated activity" severity failure;
        end procedure;

        procedure set_byte_address(byte_offset : natural) is
        begin
            assert byte_offset mod 4 = 0 and byte_offset / 4 <= 16#ffffff#
                report "invalid host byte offset" severity failure;
            av_address <= std_logic_vector(to_unsigned(byte_offset / 4, 32));
        end procedure;

        procedure write_register(
            byte_offset : natural;
            value : std_logic_vector(31 downto 0)
        ) is
            variable waited : natural := 0;
        begin
            wait until falling_edge(clock);
            set_byte_address(byte_offset);
            av_write_data <= value;
            av_write <= '1';
            loop
                wait until falling_edge(clock);
                exit when av_wait = '1';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "integrated write never entered wait" severity failure;
            end loop;
            waited := 0;
            loop
                wait until falling_edge(clock);
                exit when av_wait = '0';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "integrated write never completed" severity failure;
            end loop;
            assert av_read_valid = '0'
                report "integrated write raised read-valid" severity failure;
            av_write <= '0';
            wait until falling_edge(clock);
        end procedure;

        procedure read_register(
            byte_offset : natural;
            expected : std_logic_vector(31 downto 0)
        ) is
            variable waited : natural := 0;
        begin
            wait until falling_edge(clock);
            set_byte_address(byte_offset);
            av_read <= '1';
            loop
                wait until falling_edge(clock);
                exit when av_wait = '1';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "integrated read never entered wait" severity failure;
            end loop;
            waited := 0;
            loop
                wait until falling_edge(clock);
                exit when av_wait = '0';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "integrated read never completed" severity failure;
            end loop;
            assert av_read_valid = '1' and av_read_data = expected
                report "integrated read result differs" severity failure;
            av_read <= '0';
            wait until falling_edge(clock);
            assert av_read_valid = '0'
                report "integrated read-valid did not clear" severity failure;
        end procedure;

        procedure begin_read(byte_offset : natural) is
            variable waited : natural := 0;
        begin
            wait until falling_edge(clock);
            set_byte_address(byte_offset);
            av_read <= '1';
            loop
                wait until falling_edge(clock);
                exit when av_wait = '1';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "reset-case read never entered wait" severity failure;
            end loop;
        end procedure;

        procedure begin_write(
            byte_offset : natural;
            value : std_logic_vector(31 downto 0)
        ) is
            variable waited : natural := 0;
        begin
            wait until falling_edge(clock);
            set_byte_address(byte_offset);
            av_write_data <= value;
            av_write <= '1';
            loop
                wait until falling_edge(clock);
                exit when av_wait = '1';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "reset-case write never entered wait" severity failure;
            end loop;
        end procedure;

        procedure wait_select_without_ack is
            variable waited : natural := 0;
        begin
            loop
                wait until falling_edge(clock);
                exit when wb_select = '1' and wb_ack = '0';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "bridge did not expose its response-wait phase"
                    severity failure;
            end loop;
        end procedure;

        procedure wait_falling_condition(
            signal condition : in std_logic;
            expected : std_logic;
            description : string
        ) is
            variable waited : natural := 0;
        begin
            loop
                wait until falling_edge(clock);
                exit when condition = expected;
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report description severity failure;
            end loop;
        end procedure;

    begin
        cycles(3);
        assert wb_ack = '0' and av_wait = '0' and apply_valid = '0'
            report "integrated startup reset was not idle" severity failure;
        release_reset;

        read_register(16#00#, P2_ID);
        read_register(16#04#, P2_CAPABILITIES);
        for channel in 0 to 3 loop
            write_register(16#10# + channel * 4, std_logic_vector(WORDS(channel)));
        end loop;
        write_register(16#20#, P2_COMMIT_MAGIC);
        assert apply_valid = '1' and apply_epoch = 1 and apply_words = WORDS
            report "integrated commit payload differs" severity failure;
        wait until falling_edge(clock);
        apply_success <= '1'; apply_done <= '1';
        wait until falling_edge(clock);
        apply_success <= '0'; apply_done <= '0';
        read_register(16#24#,
            p2_status('0', '1', '1', '0', P2_REASON_NONE, "0000"));
        read_register(16#28#, x"00000001");
        read_register(16#30#, std_logic_vector(WORDS(0)));

        -- Reset after the master request becomes visible, before slave capture.
        begin_write(16#10#, x"cafef00d");
        wait_falling_condition(wb_cycle, '1', "request cycle did not appear");
        assert_skewed_reset;
        release_reset;
        read_register(16#10#, x"00000000");

        -- Reset after the slave captures a write, while sel awaits response.
        begin_write(16#10#, x"cafef00d");
        wait_select_without_ack;
        assert_skewed_reset;
        release_reset;
        read_register(16#10#, x"00000000");

        -- Reset after ACK assertion but before the bridge samples it.
        begin_read(16#00#);
        wait_falling_condition(wb_ack, '1', "integrated ACK did not appear");
        assert_skewed_reset;
        release_reset;
        read_register(16#00#, P2_ID);
        read_register(16#24#,
            p2_status('0', '0', '0', '0', P2_REASON_NONE, "0000"));
        assert monitored_requests = monitored_acks
            report "final integrated request/ACK totals differ" severity failure;

        report "exact-master/profile-2 register integration contract passed"
            severity note;
        finish;
    end process;

    watchdog : process
    begin
        wait for 30 us;
        assert false report "profile-2 Wishbone integration bench timed out"
            severity failure;
    end process;
end architecture;
