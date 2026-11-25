-- Qualify the exact pinned Nuand Avalon-to-Wishbone bridge against an authored
-- always-terminating slave. The planned profile-2 top ties both bridge clocks to
-- sys_clock, as this bench does. The pinned RTL's Wishbone process is sensitive
-- to its Avalon clock even though it tests rising_edge(wb_clk_i), so this result
-- does not claim unrelated-clock operation.
--
-- The exported handshake is unusual: stb is constant high, cyc marks the brief
-- address/data window, and sel marks the later response wait. This bench
-- validates that same-clock digital protocol only; the small dcfifo model does
-- not represent Intel FIFO CDC, timing or metastability.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

entity rf_wishbone_master_contract_tb is end entity;

architecture simulation of rf_wishbone_master_contract_tb is
    constant UNKNOWN_READ : std_logic_vector(31 downto 0) := x"bad0add0";
    constant MAX_WAIT_CYCLES : positive := 64;

    signal clock : std_logic := '0';
    signal qsys_reset : std_logic := '1';
    signal conduit_reset : std_logic := '1';
    signal slave_reset : std_logic := '1';

    signal av_address : std_logic_vector(31 downto 0) := (others => '0');
    signal av_write_data : std_logic_vector(31 downto 0) := (others => '0');
    signal av_read_data : std_logic_vector(31 downto 0);
    signal av_write, av_read : std_logic := '0';
    signal av_wait, av_read_valid, interrupt : std_logic;

    signal wb_address, wb_write_data, wb_read_data : std_logic_vector(31 downto 0);
    signal wb_write, wb_select, wb_strobe, wb_ack, wb_cycle : std_logic;

    type words_t is array (0 to 3) of std_logic_vector(31 downto 0);
    signal shadow_words : words_t := (others => (others => '0'));
    signal slave_requests, slave_acknowledgements : natural := 0;
    signal shadow_write_side_effects : natural := 0;
    signal monitored_requests, monitored_acknowledgements : natural := 0;
    signal saw_select_without_cycle : boolean := false;
begin
    clock <= not clock after 5 ns;

    bridge : entity work.wishbone_master
        generic map (ADDR_BITS => 32, DATA_BITS => 32)
        port map (
            clock => clock, reset => qsys_reset,
            addr => av_address, din => av_write_data, dout => av_read_data,
            write => av_write, read => av_read, waitreq => av_wait,
            readack => av_read_valid, intr => interrupt,
            wb_clk_i => clock, wb_rst_i => conduit_reset,
            wb_adr_o => wb_address, wb_dat_o => wb_write_data,
            wb_dat_i => wb_read_data, wb_we_o => wb_write,
            wb_sel_o => wb_select, wb_stb_o => wb_strobe,
            wb_ack_i => wb_ack, wb_cyc_o => wb_cycle
        );

    slave : process(clock, slave_reset)
        variable request_active : boolean := false;
        variable pending : boolean := false;
        variable delay_cycles : natural range 0 to 3 := 0;
        variable saved_address : natural := 0;
        variable saved_data : std_logic_vector(31 downto 0) := (others => '0');
        variable saved_write : std_logic := '0';
    begin
        if slave_reset = '1' then
            wb_ack <= '0';
            wb_read_data <= (others => '0');
            shadow_words <= (others => (others => '0'));
            request_active := false;
            pending := false;
            delay_cycles := 0;
            slave_requests <= 0;
            slave_acknowledgements <= 0;
            shadow_write_side_effects <= 0;
        elsif rising_edge(clock) then
            wb_ack <= '0';
            if wb_cycle = '0' then
                request_active := false;
            end if;
            if wb_cycle = '1' and wb_strobe = '1'
                and not request_active and not pending then
                request_active := true;
                pending := true;
                delay_cycles := 2;
                saved_address := to_integer(unsigned(wb_address(23 downto 0)));
                saved_data := wb_write_data;
                saved_write := wb_write;
                slave_requests <= slave_requests + 1;
            end if;
            -- Capture belongs to cyc/stb, but ACK is legal only after the
            -- bridge enters its separate response-wait state on sel.
            if pending and wb_select = '1' then
                if delay_cycles > 0 then
                    delay_cycles := delay_cycles - 1;
                else
                    if saved_write = '1' then
                        if saved_address >= 4 and saved_address <= 7 then
                            shadow_words(saved_address - 4) <= saved_data;
                            shadow_write_side_effects <= shadow_write_side_effects + 1;
                        end if;
                        wb_read_data <= (others => '0');
                    elsif saved_address = 0 then
                        wb_read_data <= x"52460201";
                    elsif saved_address >= 4 and saved_address <= 7 then
                        wb_read_data <= shadow_words(saved_address - 4);
                    else
                        wb_read_data <= UNKNOWN_READ;
                    end if;
                    wb_ack <= '1';
                    slave_acknowledgements <= slave_acknowledgements + 1;
                    pending := false;
                end if;
            end if;
        end if;
    end process;

    independent_monitor : process(clock)
        variable request_open : boolean := false;
        variable cycles_open : natural := 0;
        variable saved_address, saved_data : std_logic_vector(31 downto 0);
        variable saved_write : std_logic;
        variable previous_ack, previous_cycle : std_logic := '0';
    begin
        if rising_edge(clock) then
            assert wb_strobe = '1' and interrupt = '0'
                report "pinned bridge constant outputs differ" severity failure;
            assert not (previous_ack = '1' and wb_ack = '1')
                report "Wishbone ACK exceeded one clock" severity failure;
            previous_ack := wb_ack;
            if qsys_reset = '1' then
                request_open := false;
                cycles_open := 0;
                previous_ack := '0';
                previous_cycle := '0';
                monitored_requests <= 0;
                monitored_acknowledgements <= 0;
                saw_select_without_cycle <= false;
            else
                if wb_cycle = '1' and previous_cycle = '0' then
                    assert wb_strobe = '1'
                        report "Wishbone cycle lacked strobe" severity failure;
                    request_open := true;
                    cycles_open := 0;
                    saved_address := wb_address;
                    saved_data := wb_write_data;
                    saved_write := wb_write;
                    monitored_requests <= monitored_requests + 1;
                elsif request_open then
                    cycles_open := cycles_open + 1;
                    if wb_cycle = '1' then
                        assert wb_address = saved_address and wb_write_data = saved_data
                            and wb_write = saved_write
                            report "Wishbone request changed during its valid window"
                            severity failure;
                    end if;
                end if;
                if wb_ack = '1' then
                    assert request_open and wb_select = '1'
                        report "ACK occurred without one selected request" severity failure;
                    assert wb_cycle = '0'
                        report "bench did not exercise ACK after cycle deassertion" severity failure;
                    assert cycles_open >= 2
                        report "bench did not exercise delayed ACK" severity failure;
                    monitored_acknowledgements <= monitored_acknowledgements + 1;
                    request_open := false;
                end if;
                if wb_select = '1' and wb_cycle = '0' then
                    saw_select_without_cycle <= true;
                end if;
                previous_cycle := wb_cycle;
            end if;
        end if;
    end process;

    stimulus : process
        procedure cycles(count : positive) is
        begin
            for index in 1 to count loop
                wait until falling_edge(clock);
            end loop;
        end procedure;

        procedure set_byte_address(byte_offset : natural) is
        begin
            assert byte_offset mod 4 = 0
                report "host Wishbone byte offset is not word aligned" severity failure;
            assert byte_offset / 4 <= 16#ffffff#
                report "host Wishbone byte offset exceeds the Qsys address port"
                severity failure;
            av_address <= std_logic_vector(to_unsigned(byte_offset / 4, 32));
        end procedure;

        procedure write_register(
            byte_offset : natural;
            value : std_logic_vector(31 downto 0);
            extra_hold_cycles : natural := 0
        ) is
            variable before_requests, before_acks : natural;
            variable wait_cycles : natural;
        begin
            before_requests := monitored_requests;
            before_acks := monitored_acknowledgements;
            wait until falling_edge(clock);
            set_byte_address(byte_offset);
            av_write_data <= value;
            av_write <= '1';
            wait_cycles := 0;
            loop
                wait until falling_edge(clock);
                exit when av_wait = '1';
                wait_cycles := wait_cycles + 1;
                assert wait_cycles < MAX_WAIT_CYCLES
                    report "write never entered the bridge wait state"
                    severity failure;
            end loop;
            wait_cycles := 0;
            loop
                wait until falling_edge(clock);
                exit when av_wait = '0';
                wait_cycles := wait_cycles + 1;
                assert wait_cycles < MAX_WAIT_CYCLES
                    report "write never completed" severity failure;
            end loop;
            for index in 1 to extra_hold_cycles loop
                wait until falling_edge(clock);
            end loop;
            assert monitored_requests = before_requests + 1
                report "held write was missing or duplicated" severity failure;
            assert monitored_acknowledgements = before_acks + 1
                report "write did not receive exactly one response" severity failure;
            assert av_read_valid = '0'
                report "write incorrectly raised read-valid" severity failure;
            av_write <= '0';
        end procedure;

        procedure read_register(
            byte_offset : natural;
            expected : std_logic_vector(31 downto 0)
        ) is
            variable before_requests, before_acks : natural;
            variable wait_cycles : natural;
        begin
            before_requests := monitored_requests;
            before_acks := monitored_acknowledgements;
            wait until falling_edge(clock);
            set_byte_address(byte_offset);
            av_read <= '1';
            wait_cycles := 0;
            loop
                wait until falling_edge(clock);
                exit when av_wait = '1';
                wait_cycles := wait_cycles + 1;
                assert wait_cycles < MAX_WAIT_CYCLES
                    report "read never entered the bridge wait state"
                    severity failure;
            end loop;
            wait_cycles := 0;
            loop
                wait until falling_edge(clock);
                exit when av_wait = '0';
                wait_cycles := wait_cycles + 1;
                assert wait_cycles < MAX_WAIT_CYCLES
                    report "read never completed" severity failure;
            end loop;
            assert av_read_valid = '1'
                report "read completion omitted read-valid" severity failure;
            assert av_read_data = expected
                report "Wishbone read returned unexpected data" severity failure;
            assert monitored_requests = before_requests + 1
                report "held read was missing or duplicated" severity failure;
            assert monitored_acknowledgements = before_acks + 1
                report "read did not receive exactly one response" severity failure;
            av_read <= '0';
            wait until falling_edge(clock);
            assert av_read_valid = '0'
                report "read-valid did not clear after the command gap"
                severity failure;
        end procedure;

    begin
        cycles(4);
        assert av_wait = '0' and av_read_valid = '0'
            and wb_cycle = '0' and wb_select = '0' and wb_ack = '0'
            report "startup reset did not hold the bridge and slave idle"
            severity failure;
        qsys_reset <= '0';
        conduit_reset <= '0';
        slave_reset <= '0';
        cycles(3);

        -- API byte offsets 0x10..0x1c arrive at the exported bus as words 4..7.
        write_register(16#10#, x"11223344");
        read_register(16#10#, x"11223344");
        -- Source review shows that the pinned bridge does not use wb_rst_i.
        -- This idle pulse checks for no visible conduit-reset side effect; it
        -- is not runtime-reset qualification. Profile 2 needs a real reset.
        conduit_reset <= '1';
        cycles(2);
        assert av_wait = '0' and av_read_valid = '0'
            and av_read_data = x"11223344"
            and wb_cycle = '0' and wb_select = '0' and wb_ack = '0'
            report "idle conduit reset had a visible bridge side effect"
            severity failure;
        conduit_reset <= '0';
        read_register(16#10#, x"11223344");
        write_register(16#14#, x"55667788");
        write_register(16#18#, x"99aabbcc");
        write_register(16#1c#, x"ddeeff00", 4);
        read_register(16#14#, x"55667788");
        read_register(16#18#, x"99aabbcc");
        read_register(16#1c#, x"ddeeff00");

        -- Unknown aligned addresses terminate, and writes to RO/unknown words
        -- have no effect on later reads.
        write_register(16#fc#, x"ffffffff");
        read_register(16#fc#, UNKNOWN_READ);
        read_register(16#10#, x"11223344");
        read_register(16#00#, x"52460201");
        write_register(16#00#, x"00000000");
        read_register(16#00#, x"52460201");

        assert saw_select_without_cycle
            report "bench did not exercise delayed ACK after cycle deassertion"
            severity failure;
        assert slave_requests = monitored_requests
            report "slave and independent request counts differ" severity failure;
        assert slave_acknowledgements = monitored_acknowledgements
            report "slave and independent ACK counts differ" severity failure;
        assert shadow_write_side_effects = 4
            report "read-only or unknown writes changed slave state" severity failure;
        assert monitored_requests = 15 and monitored_acknowledgements = 15
            report "unexpected terminal Wishbone transaction counts" severity failure;
        report "pinned Wishbone master same-clock compatibility contract passed"
            severity note;
        finish;
    end process;

    watchdog : process
    begin
        wait for 10 us;
        assert false report "Wishbone contract bench timed out" severity failure;
    end process;
end architecture;
