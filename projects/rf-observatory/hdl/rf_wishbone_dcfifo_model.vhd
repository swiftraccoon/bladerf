-- Bounded simulation model for the two show-ahead dcfifo instances inside the
-- pinned Nuand Wishbone master. This is deliberately not an Intel FIFO timing,
-- metastability or synthesis model. It exists only to exercise the exact
-- bridge RTL and an authored terminating slave under GHDL without Quartus.

library ieee;
use ieee.std_logic_1164.all;

package altera_mf_components is
    component dcfifo is
        generic (
            lpm_width     : positive := 1;
            lpm_widthu    : positive := 1;
            lpm_numwords  : positive := 2;
            lpm_showahead : string := "OFF"
        );
        port (
            aclr    : in  std_logic := '0';
            wrclk   : in  std_logic;
            wrreq   : in  std_logic;
            data    : in  std_logic_vector(lpm_width - 1 downto 0);
            wrfull  : out std_logic;
            wrempty : out std_logic;
            wrusedw : out std_logic_vector(lpm_widthu - 1 downto 0);
            rdclk   : in  std_logic;
            rdreq   : in  std_logic;
            q       : out std_logic_vector(lpm_width - 1 downto 0);
            rdfull  : out std_logic;
            rdempty : out std_logic;
            rdusedw : out std_logic_vector(lpm_widthu - 1 downto 0)
        );
    end component;
end package;

package body altera_mf_components is end package body;

library ieee;
use ieee.std_logic_1164.all;

entity dcfifo is
    generic (
        lpm_width     : positive := 1;
        lpm_widthu    : positive := 1;
        lpm_numwords  : positive := 2;
        lpm_showahead : string := "OFF"
    );
    port (
        aclr    : in  std_logic := '0';
        wrclk   : in  std_logic;
        wrreq   : in  std_logic;
        data    : in  std_logic_vector(lpm_width - 1 downto 0);
        wrfull  : out std_logic;
        wrempty : out std_logic;
        wrusedw : out std_logic_vector(lpm_widthu - 1 downto 0);
        rdclk   : in  std_logic;
        rdreq   : in  std_logic;
        q       : out std_logic_vector(lpm_width - 1 downto 0);
        rdfull  : out std_logic;
        rdempty : out std_logic;
        rdusedw : out std_logic_vector(lpm_widthu - 1 downto 0)
    );
end entity;

architecture simulation of dcfifo is
    signal used_words : natural range 0 to lpm_numwords := 0;
begin
    assert lpm_showahead = "ON"
        report "Wishbone contract model supports only show-ahead dcfifo"
        severity failure;

    wrfull <= '1' when used_words = lpm_numwords else '0';
    rdfull <= '1' when used_words = lpm_numwords else '0';
    wrempty <= '1' when used_words = 0 else '0';
    rdempty <= '1' when used_words = 0 else '0';
    -- The pinned bridge leaves both used-word outputs open.
    wrusedw <= (others => '0');
    rdusedw <= (others => '0');

    storage : process(aclr, wrclk, rdclk)
        type words_t is array (0 to lpm_numwords - 1)
            of std_logic_vector(lpm_width - 1 downto 0);
        variable words : words_t := (others => (others => '0'));
        variable write_index : natural range 0 to lpm_numwords - 1 := 0;
        variable read_index : natural range 0 to lpm_numwords - 1 := 0;
        variable count : natural range 0 to lpm_numwords := 0;
    begin
        if aclr = '1' then
            words := (others => (others => '0'));
            write_index := 0;
            read_index := 0;
            count := 0;
            used_words <= 0;
            q <= (others => '0');
        else
            if rising_edge(wrclk) and wrreq = '1' and count < lpm_numwords then
                words(write_index) := data;
                write_index := (write_index + 1) mod lpm_numwords;
                count := count + 1;
            end if;
            if rising_edge(rdclk) and rdreq = '1' and count > 0 then
                read_index := (read_index + 1) mod lpm_numwords;
                count := count - 1;
            end if;
            used_words <= count;
            if count > 0 then
                q <= words(read_index);
            else
                -- Do not make a popped word look valid after empty. A slave
                -- must capture the request while the master's cycle is high.
                q <= (others => '0');
            end if;
        end if;
    end process;
end architecture;
